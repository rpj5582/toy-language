use cranelift::{
    codegen::ir::{
        condcodes::{FloatCC, IntCC},
        AbiParam, InstBuilder, Value,
    },
    frontend::FunctionBuilder,
};

use crate::{
    parser::{
        expression::{
            BinaryMathOperationType, BooleanComparisonType, Expression, UnaryMathOperationType,
        },
        function::FunctionSignature,
        identifier::Identifier,
        types::{DataType, Type},
        variable::Variable,
    },
    semantic::EXPECT_VAR_TYPE,
    semantic_assert,
};

use super::block::BlockVariables;

/// Represents the value of an evaluated [`Expression`].
///
/// This type capture's Cranelift's [`Value`] type and wraps it with a [`DataType`]
/// to keep track of the type when emitting type-specific codegen.
/// It is intended to be used in places where it semantically makes sense to represent
/// an expression's resulting value after evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct ExpressionValue {
    val: Value,
    ty: DataType,
}

impl From<ExpressionValue> for Value {
    fn from(value: ExpressionValue) -> Self {
        value.val
    }
}

/// Helper type that is responsible for generating Cranelift IR from expressions.
pub(super) struct ExpressionGenerator<
    'module,
    'ctx: 'builder,
    'builder,
    'block,
    M: cranelift_module::Module + 'module,
> {
    builder: &'builder mut FunctionBuilder<'ctx>,
    module: &'module mut M,
    block_vars: &'block mut BlockVariables,
}

impl<'module, 'ctx: 'builder, 'builder, 'block, M: cranelift_module::Module>
    ExpressionGenerator<'module, 'ctx, 'builder, 'block, M>
{
    pub(super) fn new(
        module: &'module mut M,
        builder: &'builder mut FunctionBuilder<'ctx>,
        block_vars: &'block mut BlockVariables,
    ) -> Self {
        Self {
            builder,
            module,
            block_vars,
        }
    }
}

impl<'module, 'ctx: 'builder, 'builder, 'var, M: cranelift_module::Module + 'module>
    ExpressionGenerator<'module, 'ctx, 'builder, 'var, M>
{
    /// Generate IR for a given expression.
    ///
    /// Returns the expression's resulting values, as well as
    /// whether or not a function return was triggered.
    ///
    /// Knowing if an inner expression triggered a function return
    /// is needed for branching expressions such as if expressions.
    ///
    /// An expression can return multiple values depending on the context.
    /// This function assumes that the context has already been checked by the [`crate::semantic`] module,
    /// so no error needs to be returned.
    pub(super) fn generate(&mut self, expression: Expression) -> (Vec<ExpressionValue>, bool) {
        match expression {
            Expression::Scope { scope, .. } => {
                let scope_block = self.builder.create_block();

                // Jump and switch to the new scope block
                self.builder.ins().jump(scope_block, &[]);
                self.builder.switch_to_block(scope_block);

                let (body, returns) = scope.split_into_return();

                for expression in body {
                    // Because we already went through semantic analysis, we know that
                    // only the last expression in the scope will actually return a value.
                    // Therefore, we don't need to care about the return values here.
                    self.generate(expression);
                }

                let mut values = Vec::new();
                let mut function_return = false;
                if let Some(returns) = returns {
                    // Get the results from the last expression in the scope if there is one
                    (values, function_return) = self.generate(returns);
                }

                self.builder.seal_block(scope_block);

                // Pass the scope's return value back as the result of the expression
                (values, function_return)
            }
            Expression::ExpressionList { expressions, .. } => {
                let mut values = Vec::new();
                let mut function_return = false;
                for expression in expressions {
                    let (mut vals, func_return) = self.generate(expression);
                    values.append(&mut vals);

                    if func_return {
                        function_return = true;
                    }
                }
                (values, function_return)
            }
            Expression::Assignment { lhs, rhs, .. } => {
                // Generate IR for the expression on the right-hand side of the equals sign
                let (values, _) = self.generate(*rhs);

                // Declare and define the variables
                let mut value_index = 0;
                for lhs_expression in lhs {
                    match lhs_expression {
                        Expression::StructMemberAccess { variable, member_names, structs } => {
                            semantic_assert!(structs.len() > 0, "structs must be populated by this point");

                            let mut index = 0;
                            for (s, member_name) in structs
                                .into_iter()
                                .zip(member_names.into_iter())
                            {
                                for struct_member in s.into_members().into_iter() {
                                    if *struct_member.name() == member_name {
                                        break;
                                    }

                                    index += DataType::from(struct_member.into_type()).primitive_types().len();
                                }
                            }

                            let block_var = &self.block_vars.block_vars(variable)[index];
                                let cranelift_variable =
                                    cranelift::frontend::Variable::from_u32(block_var.index);

                                self.builder
                                    .def_var(cranelift_variable, values[value_index].clone().into());

                                value_index += 1;
                        },
                        Expression::Variable(variable) => {
                            // Only declare and define variables if they are not discarded
                            if !variable.is_discarded() {
                                let block_vars = self.block_vars.block_vars(variable);
                                for block_var in block_vars {
                                    let cranelift_variable =
                                        cranelift::frontend::Variable::from_u32(block_var.index);

                                    // Intentionally ignore the error, since we don't care if the variable has already been declared
                                    let _ = self
                                        .builder
                                        .try_declare_var(cranelift_variable, block_var.ty.into());

                                    self.builder
                                        .def_var(cranelift_variable, values[value_index].clone().into());

                                    value_index += 1;
                                }
                            }
                        },
                        _ => unreachable!("parsing guarantees no other expressions are possible on the left hand side")
                    }
                }

                (vec![], false)
            }
            Expression::StructInstantiation {
                members, _struct, ..
            } => {
                let mut values = Vec::new();

                for (_, member_expression) in members {
                    let (mut member_values, _) = self.generate(member_expression);
                    values.append(&mut member_values);
                }

                (values, false)
            }
            Expression::StructMemberAccess {
                variable,
                member_names,
                structs,
            } => {
                semantic_assert!(structs.len() > 0, "structs must be populated by this point");

                let mut index = 0;
                let mut ty = None;
                let len = structs.len();
                for (struct_index, (s, member_name)) in structs
                    .into_iter()
                    .zip(member_names.into_iter())
                    .enumerate()
                {
                    for struct_member in s.into_members().into_iter() {
                        if *struct_member.name() == member_name {
                            if struct_index == len - 1 {
                                ty = Some(struct_member.into_type().into());
                            }

                            break;
                        }

                        index += DataType::from(struct_member.into_type())
                            .primitive_types()
                            .len();
                    }
                }

                semantic_assert!(ty != None, "struct member must have a type");

                let block_var = &self.block_vars.block_vars(variable)[index];

                (
                    vec![ExpressionValue {
                        ty: ty.unwrap(),
                        val: self
                            .builder
                            .use_var(cranelift::frontend::Variable::from_u32(block_var.index)),
                    }],
                    false,
                )
            }
            Expression::FunctionReturn { expression, .. } => {
                // Generate IR for the return value expression
                let (return_values, _) = self.generate(*expression);

                // Add the IR instruction to actually return from the function
                self.builder.ins().return_(
                    &return_values
                        .into_iter()
                        .map(|value| value.into())
                        .collect::<Vec<Value>>(),
                );

                (vec![], true)
            }
            Expression::FunctionCall {
                name,
                argument_expression,
                function_signature,
                ..
            } => {
                semantic_assert!(
                    function_signature.is_some(),
                    "function signature should be set by this point"
                );
                self.generate_function_call(name, *argument_expression, function_signature.unwrap())
            }
            Expression::IfElse {
                cond_expression,
                then_expression,
                else_expression,
                ..
            } => {
                let (cond_values, _) = self.generate(*cond_expression);
                semantic_assert!(
                    cond_values.len() == 1,
                    "if condition did not return a single value"
                );
                let cond_value = cond_values.into_iter().nth(0).unwrap();

                let then_block = self.builder.create_block();
                let else_block = self.builder.create_block();
                let merge_block = self.builder.create_block();

                self.builder
                    .ins()
                    .brif(cond_value.into(), then_block, &[], else_block, &[]);

                self.builder.switch_to_block(then_block);
                self.builder.seal_block(then_block);

                let (then_values, then_func_return) = self.generate(*then_expression);
                if !then_func_return {
                    self.builder.ins().jump(
                        merge_block,
                        &then_values
                            .iter()
                            .map(|val| val.clone().into())
                            .collect::<Vec<Value>>(),
                    );
                }

                let mut else_values = Vec::new();
                let mut else_func_return = false;
                if let Some(else_expression) = *else_expression {
                    self.builder.switch_to_block(else_block);
                    self.builder.seal_block(else_block);

                    (else_values, else_func_return) = self.generate(else_expression);
                    if !else_func_return {
                        self.builder.ins().jump(
                            merge_block,
                            &else_values
                                .iter()
                                .map(|val| val.clone().into())
                                .collect::<Vec<Value>>(),
                        );
                    }
                }

                let mut result_values = Vec::new();
                if !then_func_return {
                    result_values = then_values;
                    for result_value in &result_values {
                        self.builder
                            .append_block_param(merge_block, result_value.ty.clone().into());
                    }
                } else if !else_values.is_empty() && !else_func_return {
                    result_values = else_values;
                    // Only add the merge block params if the then block did a function return and the else block didn't
                    for result_value in &result_values {
                        self.builder
                            .append_block_param(merge_block, result_value.ty.clone().into());
                    }
                }

                self.builder.switch_to_block(merge_block);
                self.builder.seal_block(merge_block);

                (result_values, false)
            }
            Expression::BooleanComparison {
                comparison_type,
                lhs,
                rhs,
                ..
            } => (
                vec![self.generate_boolean_comparison(comparison_type, *lhs, *rhs)],
                false,
            ),
            Expression::BinaryMathOperation {
                operation_type,
                lhs,
                rhs,
                ..
            } => (
                vec![self.generate_binary_operation(operation_type, *lhs, *rhs)],
                false,
            ),
            Expression::UnaryMathOperation {
                operation_type,
                expression,
                ..
            } => (
                vec![self.generate_unary_operation(operation_type, *expression)],
                false,
            ),
            Expression::Variable(variable) => (self.generate_variable(variable), false),
            Expression::IntLiteral(literal) => {
                (vec![self.generate_int_literal(literal.val())], false)
            }
            Expression::FloatLiteral(literal) => {
                (vec![self.generate_float_literal(literal.val())], false)
            }
            Expression::BoolLiteral(literal) => {
                (vec![self.generate_bool_literal(literal.val())], false)
            }
        }
    }

    fn generate_boolean_comparison(
        &mut self,
        comparison_type: BooleanComparisonType,
        lhs: Expression,
        rhs: Expression,
    ) -> ExpressionValue {
        let (left_values, _) = self.generate(lhs);
        let (right_values, _) = self.generate(rhs);
        semantic_assert!(
            left_values.len() == 1,
            "left boolean operand expression did not return a single value"
        );
        semantic_assert!(
            right_values.len() == 1,
            "right boolean operand expression did not return a single value"
        );

        let left_value = left_values.into_iter().nth(0).unwrap();
        let right_value = right_values.into_iter().nth(0).unwrap();
        semantic_assert!(
            left_value.ty == right_value.ty,
            "lhs and rhs types of boolean operation are mismatched"
        );

        match comparison_type {
            BooleanComparisonType::Equal => match left_value.ty {
                DataType::Int | DataType::Bool => ExpressionValue {
                    val: self.builder.ins().icmp(
                        IntCC::Equal,
                        left_value.into(),
                        right_value.into(),
                    ),
                    ty: DataType::Bool,
                },
                DataType::Float => ExpressionValue {
                    val: self.builder.ins().fcmp(
                        FloatCC::Equal,
                        left_value.into(),
                        right_value.into(),
                    ),
                    ty: DataType::Bool,
                },
                DataType::Struct { .. } => {
                    unreachable!("attempted to compare non-comparable values")
                }
            },
            BooleanComparisonType::NotEqual => match left_value.ty {
                DataType::Int | DataType::Bool => ExpressionValue {
                    val: self.builder.ins().icmp(
                        IntCC::NotEqual,
                        left_value.into(),
                        right_value.into(),
                    ),
                    ty: DataType::Bool,
                },
                DataType::Float => ExpressionValue {
                    val: self.builder.ins().fcmp(
                        FloatCC::NotEqual,
                        left_value.into(),
                        right_value.into(),
                    ),
                    ty: DataType::Bool,
                },
                DataType::Struct { .. } => {
                    unreachable!("attempted to compare non-comparable values")
                }
            },
            BooleanComparisonType::LessThan => match left_value.ty {
                DataType::Int | DataType::Bool => ExpressionValue {
                    val: self.builder.ins().icmp(
                        IntCC::SignedLessThan,
                        left_value.into(),
                        right_value.into(),
                    ),
                    ty: DataType::Bool,
                },
                DataType::Float => ExpressionValue {
                    val: self.builder.ins().fcmp(
                        FloatCC::LessThan,
                        left_value.into(),
                        right_value.into(),
                    ),
                    ty: DataType::Bool,
                },
                DataType::Struct { .. } => {
                    unreachable!("attempted to compare non-comparable values")
                }
            },
            BooleanComparisonType::LessThanEqual => match left_value.ty {
                DataType::Int | DataType::Bool => ExpressionValue {
                    val: self.builder.ins().icmp(
                        IntCC::SignedLessThanOrEqual,
                        left_value.into(),
                        right_value.into(),
                    ),
                    ty: DataType::Bool,
                },
                DataType::Float => ExpressionValue {
                    val: self.builder.ins().fcmp(
                        FloatCC::LessThanOrEqual,
                        left_value.into(),
                        right_value.into(),
                    ),
                    ty: DataType::Bool,
                },
                DataType::Struct { .. } => {
                    unreachable!("attempted to compare non-comparable values")
                }
            },
            BooleanComparisonType::GreaterThan => match left_value.ty {
                DataType::Int | DataType::Bool => ExpressionValue {
                    val: self.builder.ins().icmp(
                        IntCC::SignedGreaterThan,
                        left_value.into(),
                        right_value.into(),
                    ),
                    ty: DataType::Bool,
                },
                DataType::Float => ExpressionValue {
                    val: self.builder.ins().fcmp(
                        FloatCC::GreaterThan,
                        left_value.into(),
                        right_value.into(),
                    ),
                    ty: DataType::Bool,
                },
                DataType::Struct { .. } => {
                    unreachable!("attempted to compare non-comparable values")
                }
            },
            BooleanComparisonType::GreaterThanEqual => match left_value.ty {
                DataType::Int | DataType::Bool => ExpressionValue {
                    val: self.builder.ins().icmp(
                        IntCC::SignedGreaterThanOrEqual,
                        left_value.into(),
                        right_value.into(),
                    ),
                    ty: DataType::Bool,
                },
                DataType::Float => ExpressionValue {
                    val: self.builder.ins().fcmp(
                        FloatCC::GreaterThanOrEqual,
                        left_value.into(),
                        right_value.into(),
                    ),
                    ty: DataType::Bool,
                },
                DataType::Struct { .. } => {
                    unreachable!("attempted to compare non-comparable values")
                }
            },
        }
    }

    fn generate_binary_operation(
        &mut self,
        operation_type: BinaryMathOperationType,
        lhs: Expression,
        rhs: Expression,
    ) -> ExpressionValue {
        let (left_values, _) = self.generate(lhs);
        let (right_values, _) = self.generate(rhs);
        semantic_assert!(
            left_values.len() == 1,
            "left binary operand expression did not return a single value"
        );
        semantic_assert!(
            right_values.len() == 1,
            "right binary operand expression did not return a single value"
        );

        let left_value = left_values.into_iter().nth(0).unwrap();
        let right_value = right_values.into_iter().nth(0).unwrap();
        semantic_assert!(
            left_value.ty == right_value.ty,
            "lhs and rhs types of binary operation are mismatched"
        );

        match operation_type {
            BinaryMathOperationType::Add => match left_value.ty {
                DataType::Int => ExpressionValue {
                    val: self
                        .builder
                        .ins()
                        .iadd(left_value.clone().into(), right_value.into()),
                    ty: left_value.ty,
                },
                DataType::Float => ExpressionValue {
                    val: self
                        .builder
                        .ins()
                        .fadd(left_value.clone().into(), right_value.into()),
                    ty: left_value.ty,
                },
                DataType::Bool => unreachable!("attempted to add non-numeric values"),
                DataType::Struct { .. } => unreachable!("attempted to add non-numeric values"),
            },
            BinaryMathOperationType::Subtract => match left_value.ty {
                DataType::Int => ExpressionValue {
                    val: self
                        .builder
                        .ins()
                        .isub(left_value.clone().into(), right_value.into()),
                    ty: left_value.ty,
                },
                DataType::Float => ExpressionValue {
                    val: self
                        .builder
                        .ins()
                        .fsub(left_value.clone().into(), right_value.into()),
                    ty: left_value.ty,
                },
                DataType::Bool => unreachable!("attempted to subtract non-numeric values"),
                DataType::Struct { .. } => unreachable!("attempted to subtract non-numeric values"),
            },
            BinaryMathOperationType::Multiply => match left_value.ty {
                DataType::Int => ExpressionValue {
                    val: self
                        .builder
                        .ins()
                        .imul(left_value.clone().into(), right_value.into()),
                    ty: left_value.ty,
                },
                DataType::Float => ExpressionValue {
                    val: self
                        .builder
                        .ins()
                        .fmul(left_value.clone().into(), right_value.into()),
                    ty: left_value.ty,
                },
                DataType::Bool => unreachable!("attempted to multiply non-numeric values"),
                DataType::Struct { .. } => unreachable!("attempted to multiply non-numeric values"),
            },
            BinaryMathOperationType::Divide => match left_value.ty {
                DataType::Int => ExpressionValue {
                    val: self
                        .builder
                        .ins()
                        .udiv(left_value.clone().into(), right_value.into()),
                    ty: left_value.ty,
                },
                DataType::Float => ExpressionValue {
                    val: self
                        .builder
                        .ins()
                        .fdiv(left_value.clone().into(), right_value.into()),
                    ty: left_value.ty,
                },
                DataType::Bool => unreachable!("attempted to divide non-numeric values"),
                DataType::Struct { .. } => unreachable!("attempted to divide non-numeric values"),
            },
        }
    }

    fn generate_unary_operation(
        &mut self,
        operation_type: UnaryMathOperationType,
        expression: Expression,
    ) -> ExpressionValue {
        let (values, _) = self.generate(expression);
        semantic_assert!(
            values.len() == 1,
            "unary operand expression did not return a single value"
        );
        match operation_type {
            UnaryMathOperationType::Negate => {
                let value = values.into_iter().nth(0).unwrap();
                match value.ty {
                    DataType::Int => ExpressionValue {
                        val: self.builder.ins().ineg(value.clone().into()),
                        ty: value.ty,
                    },
                    DataType::Float => ExpressionValue {
                        val: self.builder.ins().fneg(value.clone().into()),
                        ty: value.ty,
                    },
                    DataType::Bool => unreachable!("attempted to negate non-numeric value"),
                    DataType::Struct { .. } => {
                        unreachable!("attempted to negate non-numeric value")
                    }
                }
            }
        }
    }

    fn generate_function_call(
        &mut self,
        name: Identifier,
        argument_expression: Option<Expression>,
        func_sig: FunctionSignature,
    ) -> (Vec<ExpressionValue>, bool) {
        // Because we've already done semantic analysis, we know that the function being called is defined,
        // and this call to that function is correct.
        // Therefore, we can build the function signature from the function call information.
        // This allows for any arbitrary function definition order.
        let mut sig = self.module.make_signature();

        for ty in func_sig.params.types() {
            let primitive_types = DataType::from(ty).primitive_types();
            for primitive_type in primitive_types {
                sig.params.push(AbiParam::new(primitive_type.into()))
            }
        }

        let mut primitive_return_types = Vec::new();
        for ty in func_sig.returns {
            let type_source = ty.source();

            let primitive_types = DataType::from(ty).primitive_types();
            for primitive_type in primitive_types {
                sig.returns
                    .push(AbiParam::new(primitive_type.clone().into()));

                // Keep track of the primitive return types so that they can be associated
                // with the function call's return values.
                // Use the same type source for each primitive type if it is expanded into multiple.
                primitive_return_types.push(Type::new(primitive_type, type_source));
            }
        }

        let func_id_to_call = self
            .module
            .declare_function(&name.to_string(), cranelift_module::Linkage::Local, &sig)
            .unwrap(); // TODO: this could have a genuine error but it's extremely unlikely, so it's not worth changing the API right now

        // Get the function to be called
        let func_ref_to_call = self
            .module
            .declare_func_in_func(func_id_to_call, self.builder.func);

        // Generate IR for the argument expression
        let mut values = Vec::new();
        if let Some(argument_expression) = argument_expression {
            let (vals, _) = self.generate(argument_expression);
            values = vals;
            semantic_assert!(
                values.len() == sig.params.len(),
                "function argument expression did not return the correct number of values"
            );
        }

        // Call function
        let call = self.builder.ins().call(
            func_ref_to_call,
            &values
                .into_iter()
                .map(|val| val.into())
                .collect::<Vec<Value>>(),
        );

        // Return function return values
        (
            self.builder
                .inst_results(call)
                .into_iter()
                .zip(primitive_return_types.into_iter())
                .map(|(value, ty)| ExpressionValue {
                    val: *value,
                    ty: ty.into(),
                })
                .collect(),
            false,
        )
    }

    fn generate_variable(&mut self, variable: Variable) -> Vec<ExpressionValue> {
        let var_data_type: DataType = variable.get_type().clone().expect(EXPECT_VAR_TYPE).into();
        let block_vars = self.block_vars.block_vars(variable);

        let mut values = Vec::with_capacity(block_vars.len());
        for block_var in block_vars {
            values.push(ExpressionValue {
                ty: var_data_type.clone(),
                val: self
                    .builder
                    .use_var(cranelift::frontend::Variable::from_u32(block_var.index)),
            });
        }

        values
    }

    fn generate_int_literal(&mut self, value: i64) -> ExpressionValue {
        ExpressionValue {
            val: self.builder.ins().iconst(DataType::Int.into(), value),
            ty: DataType::Int,
        }
    }

    fn generate_float_literal(&mut self, value: f64) -> ExpressionValue {
        ExpressionValue {
            val: self.builder.ins().f64const(value),
            ty: DataType::Float,
        }
    }

    fn generate_bool_literal(&mut self, value: bool) -> ExpressionValue {
        ExpressionValue {
            val: self
                .builder
                .ins()
                .iconst(DataType::Bool.into(), if value { 1 } else { 0 }),
            ty: DataType::Bool,
        }
    }
}
