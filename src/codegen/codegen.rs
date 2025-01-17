use cranelift::{
    codegen::ir::AbiParam,
    frontend::{FunctionBuilder, FunctionBuilderContext, Variable as CraneliftVariable},
};
use thiserror::Error;

use crate::{
    parser::{
        ast::AbstractSyntaxTree, definition::Definition, function::Function, types::DataType,
    },
    semantic::EXPECT_VAR_TYPE,
};

use super::{
    block::BlockVariables,
    context::CodeGenContext,
    expression_generator::ExpressionGenerator,
    options::{CodeGenOptions, OptimizationLevel},
};

/// An error that captures all errors that can be thrown during code generation.
#[derive(Debug, Error)]
pub(crate) enum CodeGenError {
    #[error("Error creating codegen module: {0}")]
    CreateError(cranelift_module::ModuleError),
    #[error("Error finalizing compilation: {0}")]
    FinalizeError(cranelift_module::ModuleError),
    #[error("Error declaring function {0}: {1}")]
    FunctionDeclarationError(String, String),
    #[error("Error defining function {0}: {1}")]
    FunctionDefinitionError(String, String),
}

/// Abstracts different types of code generation backends that can be used by [`CodeGenerator`].
///
/// The main purpose for this abstraction is to support JIT and AOT compilation methods.
/// [`cranelift_module::Module`] takes care of most of this for us,
/// but some additional functions are needed to abstract specifics surrounding
/// emitting generated code, so that [`CodeGenerator`] can work independently of the backend.
/// Currently, only [`cranelift_jit::JITModule`] is implemented.
pub(crate) trait CodeGeneratorModule: cranelift_module::Module {
    fn finalize(&mut self) -> Result<(), CodeGenError>;
}

impl CodeGeneratorModule for cranelift_jit::JITModule {
    fn finalize(&mut self) -> Result<(), CodeGenError> {
        self.finalize_definitions()
            .map_err(|err| CodeGenError::FinalizeError(err))?;

        Ok(())
    }
}

/// A wrapper type for generating Cranelift IR, and then machine code.
pub(crate) struct CodeGenerator<M: CodeGeneratorModule> {
    module: M,
    context: CodeGenContext,
    request_ir: bool,
    main_function_id: Option<cranelift_module::FuncId>,

    ir: Option<String>,
    disassembly: Option<String>,
}

pub(crate) struct JitCodeGenResults {
    pub(crate) code: *const u8,
    pub(crate) ir: Option<String>,
    pub(crate) disassembly: Option<String>,
}

impl CodeGenerator<cranelift_jit::JITModule> {
    /// Creates a new [`CodeGenerator`] instance with a given [`OptimizationLevel`].
    pub(crate) fn new(options: &CodeGenOptions) -> Result<Self, CodeGenError> {
        let builder = match options.optimization_level {
            OptimizationLevel::None => cranelift_jit::JITBuilder::with_flags(
                &[("opt_level", "none")],
                cranelift_module::default_libcall_names(),
            ),
            OptimizationLevel::Speed => cranelift_jit::JITBuilder::with_flags(
                &[("opt_level", "speed")],
                cranelift_module::default_libcall_names(),
            ),
            OptimizationLevel::SpeedAndSize => cranelift_jit::JITBuilder::with_flags(
                &[("opt_level", "speed_and_size")],
                cranelift_module::default_libcall_names(),
            ),
        }
        .map_err(|err| CodeGenError::CreateError(err))?;

        let module = cranelift_jit::JITModule::new(builder);
        let context = CodeGenContext::new(&module, options.request_disassembly);

        Ok(Self {
            module,
            context,
            request_ir: options.request_ir,
            main_function_id: None,
            ir: None,
            disassembly: None,
        })
    }

    pub(crate) fn results(self) -> JitCodeGenResults {
        JitCodeGenResults {
            code: self
                .module
                .get_finalized_function(self.main_function_id.expect("missing main function")),
            ir: self.ir,
            disassembly: self.disassembly,
        }
    }

    /// Returns a pointer to the main function after the machine code is generated,
    /// or [`None`] if the machine code has not been successfully generated yet.
    pub(crate) fn get_main_function(&self) -> Option<*const u8> {
        match self.main_function_id {
            Some(main_function_id) => Some(self.module.get_finalized_function(main_function_id)),
            None => None,
        }
    }
}

impl<M: CodeGeneratorModule> CodeGenerator<M> {
    /// Generate Cranelift IR from the given [`AbstractSyntaxTree`], and then compile that to machine code.
    ///
    /// Accessing the resulting machine code will depend on the implementation of the underlying [`CodeGeneratorModule`].
    /// For instance, a JIT module would give access to the main function in memory, while an object module
    /// would provide the machine code in object files so that they can be linked by a linker.
    ///
    /// Because semantic analysis has already validated the source code, this function only returns a [`CodeGenError`]
    /// when something is fundamentally wrong. This could be due to lack of resources, or a bug in the compiler.
    ///
    /// If the intermediate IR is requested, it can be retrieved with the [`ir()`] function.
    /// If a disassembly is requested, it can be retrieved with the [`disassembly()`] function.
    pub(crate) fn generate(&mut self, ast: AbstractSyntaxTree) -> Result<(), CodeGenError> {
        let mut ir = String::new();
        let mut disassembly = String::new();

        // Generate each function individually, and build up the IR and/or disassembly if requested
        let mut function_context = FunctionBuilderContext::new();
        for definition in ast {
            match definition {
                Definition::Struct(_struct) => {}
                Definition::Function(function) => {
                    // Only generate functions if they are not discarded
                    if !function.signature.is_discarded() {
                        let (function_ir, function_disasm) =
                            self.generate_function(&mut function_context, function)?;

                        match function_ir {
                            Some(function_ir) => ir.push_str(format!("{}\n", function_ir).as_str()),
                            None => {}
                        }

                        match function_disasm {
                            Some(function_disasm) => {
                                disassembly.push_str(format!("{}\n", function_disasm).as_str())
                            }
                            None => {}
                        }
                    }
                }
            }
        }

        // Finalize the compilation in a module-independent way
        self.module.finalize()?;

        if !ir.is_empty() {
            self.ir = Some(ir)
        }

        if !disassembly.is_empty() {
            self.disassembly = Some(disassembly)
        }

        Ok(())
    }

    fn generate_function(
        &mut self,
        function_context: &mut FunctionBuilderContext,
        function: Function,
    ) -> Result<(Option<String>, Option<String>), CodeGenError> {
        // Pull out the wrapped context
        let context = self.context.get_inner_context_mut();

        let Function { signature, scope } = function;

        // Add the function's parameters to the context
        for param in &signature.params {
            let primitive_types =
                DataType::from(param.get_type().clone().expect(EXPECT_VAR_TYPE)).primitive_types();
            for primitive_type in primitive_types {
                context
                    .func
                    .signature
                    .params
                    .push(AbiParam::new(primitive_type.clone().into()));
            }
        }

        // Add the function's return types to the context
        for return_type in signature.returns {
            let primitive_types = DataType::from(return_type).primitive_types();
            for primitive_type in primitive_types {
                context
                    .func
                    .signature
                    .returns
                    .push(AbiParam::new(primitive_type.into()));
            }
        }

        let function_name = signature.name.to_string();

        // We can now declare the function to Cranelift from the context
        let function_id = self
            .module
            .declare_function(
                &function_name,
                cranelift_module::Linkage::Local,
                &context.func.signature,
            )
            .map_err(|err| {
                Self::check_codegen_error(&err);
                CodeGenError::FunctionDeclarationError(function_name.clone(), err.to_string())
            })?;

        // Instantiate the function builder and create the function entry block where IR code will be emitted.
        // Because this is the entry block of the function, we can seal it early as no other blocks can branch to it.
        let mut builder = FunctionBuilder::new(&mut context.func, function_context);
        let function_block = builder.create_block();
        builder.append_block_params_for_function_params(function_block);
        builder.switch_to_block(function_block);
        builder.seal_block(function_block);

        // Prime the block variables with the function parameters
        let mut block_vars = BlockVariables::new();
        for function_param in signature.params.into_iter() {
            let b_vars = block_vars.block_vars(function_param);
            for b_var in b_vars {
                let cranelift_variable = CraneliftVariable::from_u32(b_var.index);

                builder.declare_var(cranelift_variable, b_var.ty.into());
                builder.def_var(
                    cranelift_variable,
                    builder.block_params(function_block)[b_var.index as usize],
                );
            }
        }

        // Now we can generate the function scope by generating each expression in the scope
        let mut expression_generator =
            ExpressionGenerator::new(&mut self.module, &mut builder, &mut block_vars);

        for expression in scope {
            expression_generator.generate(expression);
        }

        builder.finalize();

        // Mark the function as defined to kick off Cranelift IR compilation
        self.module
            .define_function(function_id, context)
            .map_err(|err| {
                Self::check_codegen_error(&err);
                CodeGenError::FunctionDefinitionError(function_name.clone(), err.to_string())
            })?;

        // Take note if this is the main function
        if function_name == "main" {
            self.main_function_id = Some(function_id);
        }

        let mut ir = None;
        let disassembly = context.compiled_code().unwrap().vcode.clone();

        if self.request_ir {
            ir = Some(context.func.to_string());
        }

        self.context.clear(&self.module);

        Ok((ir, disassembly))
    }

    /// Check if a given module error is caused by an error in the IR code, or a bug in the code generation.
    ///
    /// Panics if the error is actually due to a bug in the compiler.
    /// This is helpful while iterating and debugging the compiler.
    fn check_codegen_error(err: &cranelift_module::ModuleError) {
        match err {
            cranelift_module::ModuleError::Compilation(ref inner_err) => match inner_err {
                cranelift::codegen::CodegenError::Verifier(inner_err) => panic!(
                    "Codegen error. This is a bug in the compiler.\n{}:\n{}",
                    err.to_string(),
                    inner_err.to_string()
                ),
                _ => {}
            },
            _ => {}
        }
    }
}
