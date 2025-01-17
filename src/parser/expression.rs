use super::{
    function::FunctionSignature, identifier::Identifier, literal::Literal, scope::Scope,
    source_range::SourceRange, variable::Variable, Struct,
};

/// Each type of boolean comparison that can be used in an expression.
///
/// This struct is useful when a boolean comparison needs to be addressed separately from an expression.
/// Using a separate comparsion type instead of binding the operands to this enum
/// makes the expression generation code cleaner.
#[derive(Debug, Clone, Copy)]
pub(crate) enum BooleanComparisonType {
    Equal,
    NotEqual,
    LessThan,
    LessThanEqual,
    GreaterThan,
    GreaterThanEqual,
}

/// Each type of binary math operation that can be used in an expression.
///
/// This works similarly to [`BooleanComparisonType`], but is used for math operations with two operands.
#[derive(Debug, Clone, Copy)]
pub(crate) enum BinaryMathOperationType {
    Add,
    Subtract,
    Multiply,
    Divide,
}

/// Each type of unary math operation that can be used in an expression.
///
/// This works similarly to [`BinaryMathOperationType`], but is used for math operations with only one operand.
#[derive(Debug, Clone, Copy)]
pub(crate) enum UnaryMathOperationType {
    Negate,
}

/// A TODO_LANG_NAME expression that, when evaluated, can return zero or more values.
///
/// Everything is an expression in TODO_LANG_NAME. This is different compared to other languages like C++ that have rvalue semantics.
/// Even assignment "statements" are expressions, they just return no values. See [`Expression::Assignment`].
///
/// In TODO_LANG_NAME, if an expression returns a value, that value must be assigned to a variable using an assignment expression,
/// otherwise the program won't compile.
/// If an expression such as a [`Expression::FunctionCall`] returns no values, then it does not need to be assigned to a variable
/// as there is nothing to assign.
///
/// It is worth noting that variables and literals are expressions as well.
/// [`Expression::IntLiteral`] is a more obvious example as it literally represents a value, but a [`Expression::Variable`]
/// is also an expression, since evaluating the variable means to return its currently stored value.
///
/// The [`Expression`] type itself does not contain any information on what value the expression returns.
/// Its value can only be determined when the expression is evaluated at runtime.
/// Instead, the type stores the information needed to perform semantic analysis and to
/// generate the Cranelift IR representing the expression.
///
/// Expressions support recursive evaluation, and in those cases the inner expression must be wrapped
/// in a container type such as [`Box`] or [`Vec`].
#[derive(Debug, Clone)]
pub(crate) enum Expression {
    /// The result of a TODO_LANG_NAME [`Scope`] can be used as an expression result.
    ///
    /// This conveniently allows for in-line processing where a function would otherwise be needed
    /// to achieve the same effect.
    Scope {
        scope: Scope,
        source: SourceRange,
    },
    /// A wrapped list of inner expressions.
    ///
    /// Having an [`Expression`] be recursive like this allows for simpler parsing of scope return values,
    /// and can be used in other places like function call arguments or assignments.
    ExpressionList {
        expressions: Vec<Expression>,
        source: SourceRange,
    },
    /// Assignments store the results of the expressions on the right hand side of the equals sign
    /// into the variables retrieved from the expressions on the left hand side of the equals sign.
    ///
    /// The expressions on the left hand side can only be [`Expression::Variable`] or [`Expression::StructMemberAccess`],
    /// otherwise the assignment is not valid and will return a compiler error.
    ///
    /// Assignments must have an equal number of expressions on the left hand side
    /// compared with the return values of the expressions on the right hand side.
    ///
    /// If the results of an expression on the right hand side are not needed, they still must be assigned to a variable.
    /// A discarded variable can be used to signify that the results are intentionally being ignored.
    Assignment {
        lhs: Vec<Expression>,
        rhs: Box<Expression>,
        source: SourceRange,
    },
    /// Represents the instantiation of an existing [`Struct`].
    ///
    /// All members of the struct need to be specified when defining a struct instantiation.
    /// This is to ensure that if a new member is added to the struct in the future,
    /// all instantiations of the struct can be easily identified and unexpected behavior will not occur.
    ///
    /// The struct that is being instantiated can't be parsed from the instantiation itself,
    /// but can be deduced during semantic analysis.
    /// Until then, the struct will have a value of [`None`].
    StructInstantiation {
        name: Identifier,
        members: Vec<(Identifier, Expression)>,
        source: SourceRange,
        _struct: Option<Struct>,
    },
    /// Access to a member of a [`Struct`].
    ///
    /// This expression can appear on the left hand side and/or the right hand side of an [`Expression::Assignment`].
    /// On the left hand side the variable is written to, and on the right hand side the variable is read from.
    ///
    /// The structs that are being accessed can't be parsed from the access itself, but can be deduced during semantic analysis.
    /// Until then, the structs will be empty.
    StructMemberAccess {
        variable: Variable,
        member_names: Vec<Identifier>,
        structs: Vec<Struct>,
    },
    /// Represents returning from a function. This is useful for early returning from an outer function scope.
    FunctionReturn {
        expression: Box<Expression>,
        source: SourceRange,
    },
    /// The called function signature can't be parsed from the function call expression itself,
    /// but can be deduced during semantic analysis.
    /// Until then, the function signature will have a value of [`None`].
    FunctionCall {
        name: Identifier,
        argument_expression: Box<Option<Expression>>,
        source: SourceRange,
        function_signature: Option<FunctionSignature>,
    },
    IfElse {
        cond_expression: Box<Expression>,
        then_expression: Box<Expression>,
        else_expression: Box<Option<Expression>>,
        source: SourceRange,
    },
    BooleanComparison {
        comparison_type: BooleanComparisonType,
        lhs: Box<Expression>,
        rhs: Box<Expression>,
        source: SourceRange,
    },
    BinaryMathOperation {
        operation_type: BinaryMathOperationType,
        lhs: Box<Expression>,
        rhs: Box<Expression>,
        source: SourceRange,
    },
    UnaryMathOperation {
        operation_type: UnaryMathOperationType,
        expression: Box<Expression>,
        source: SourceRange,
    },
    Variable(Variable),
    IntLiteral(Literal<i64>),
    FloatLiteral(Literal<f64>),
    BoolLiteral(Literal<bool>),
}

impl Expression {
    /// Returns a [`SourceRange`] that captures the expression.
    pub(crate) fn source(&self) -> SourceRange {
        match self {
            Expression::Scope { source, .. } => *source,
            Expression::ExpressionList {
                expressions,
                source,
            } => {
                // Prefer combining the first and last expression source ranges,
                // as that gives a better looking source range for diagnostic messages
                if expressions.len() > 0 {
                    expressions
                        .first()
                        .unwrap()
                        .source()
                        .combine(expressions.last().unwrap().source())
                } else {
                    *source
                }
            }
            Expression::Assignment { source, .. } => *source,
            Expression::StructInstantiation { source, .. } => *source,
            Expression::StructMemberAccess {
                variable,
                member_names,
                ..
            } => {
                if let Some(last_member) = member_names.last() {
                    variable.source().combine(last_member.source())
                } else {
                    variable.source()
                }
            }
            Expression::FunctionReturn { source, .. } => *source,
            Expression::FunctionCall { source, .. } => *source,
            Expression::IfElse { source, .. } => *source,
            Expression::BooleanComparison { source, .. } => *source,
            Expression::BinaryMathOperation { source, .. } => *source,
            Expression::UnaryMathOperation { source, .. } => *source,
            Expression::Variable(variable) => variable.source(),
            Expression::IntLiteral(literal) => literal.source(),
            Expression::FloatLiteral(literal) => literal.source(),
            Expression::BoolLiteral(literal) => literal.source(),
        }
    }

    /// Returns the innermost expression if this expression wraps an expression transparently,
    /// otherwise return self.
    ///
    /// Wrapping an expression transparently means that the wrapping expression does not change
    /// the semantics of the expression it wraps.
    /// An example of this is an [`Expression::ExpressionList`] with only one inner expression.
    pub(crate) fn unwrap_transparent(&self) -> &Expression {
        match self {
            Expression::Scope { scope, .. } => {
                if scope.len() != 1 {
                    self
                } else {
                    scope.first().unwrap().unwrap_transparent()
                }
            }
            Expression::ExpressionList { expressions, .. } => {
                if expressions.len() != 1 {
                    self
                } else {
                    expressions.first().unwrap().unwrap_transparent()
                }
            }
            _ => self,
        }
    }

    /// Returns the last [`Expression::FunctionReturn`] contained in this expression,
    /// or [`None`] if this expression is not and does not contain an [`Expression::FunctionReturn`].
    pub(crate) fn last_function_return(&self) -> Option<&Self> {
        let expression = self.unwrap_transparent();
        match expression {
            Expression::Scope { scope, .. } => {
                if let Some((returns, _)) = scope.split_return() {
                    let returns = returns.unwrap_transparent();
                    match returns {
                        Expression::FunctionReturn { .. } => Some(returns),
                        _ => None,
                    }
                } else {
                    None
                }
            }
            Expression::FunctionReturn { .. } => Some(expression),
            _ => None,
        }
    }
}
