use crate::{current_dir_str, get_full_help, scope::create_scope};
use nu_path::expand_path_with;
use nu_protocol::{
    ast::{
        Argument, Assignment, Bits, Block, Boolean, Call, Comparison, Expr, Expression, Math,
        Operator, PathMember, PipelineElement, Redirection,
    },
    engine::{EngineState, Stack},
    Config, HistoryFileFormat, IntoInterruptiblePipelineData, IntoPipelineData, PipelineData,
    Range, ShellError, Span, Spanned, Unit, Value, VarId, ENV_VARIABLE_ID,
};
use nu_utils::stdout_write_all_and_flush;
use std::collections::HashMap;
use sysinfo::SystemExt;

pub fn eval_operator(op: &Expression) -> Result<Operator, ShellError> {
    match op {
        Expression {
            expr: Expr::Operator(operator),
            ..
        } => Ok(operator.clone()),
        Expression { span, expr, .. } => {
            Err(ShellError::UnknownOperator(format!("{:?}", expr), *span))
        }
    }
}

pub fn eval_call(
    engine_state: &EngineState,
    caller_stack: &mut Stack,
    call: &Call,
    input: PipelineData,
) -> Result<PipelineData, ShellError> {
    if nu_utils::ctrl_c::was_pressed(&engine_state.ctrlc) {
        return Ok(Value::Nothing { span: call.head }.into_pipeline_data());
    }
    let decl = engine_state.get_decl(call.decl_id);

    if !decl.is_known_external() && call.named_iter().any(|(flag, _, _)| flag.item == "help") {
        let mut signature = decl.signature();
        signature.usage = decl.usage().to_string();
        signature.extra_usage = decl.extra_usage().to_string();

        let full_help = get_full_help(
            &signature,
            &decl.examples(),
            engine_state,
            caller_stack,
            decl.is_parser_keyword(),
        );
        Ok(Value::String {
            val: full_help,
            span: call.head,
        }
        .into_pipeline_data())
    } else if let Some(block_id) = decl.get_block_id() {
        let block = engine_state.get_block(block_id);

        let mut callee_stack = caller_stack.gather_captures(&block.captures);

        for (param_idx, param) in decl
            .signature()
            .required_positional
            .iter()
            .chain(decl.signature().optional_positional.iter())
            .enumerate()
        {
            let var_id = param
                .var_id
                .expect("internal error: all custom parameters must have var_ids");

            if let Some(arg) = call.positional_nth(param_idx) {
                let result = eval_expression(engine_state, caller_stack, arg)?;
                callee_stack.add_var(var_id, result);
            } else if let Some(arg) = &param.default_value {
                let result = eval_expression(engine_state, caller_stack, arg)?;
                callee_stack.add_var(var_id, result);
            } else {
                callee_stack.add_var(var_id, Value::nothing(call.head));
            }
        }

        if let Some(rest_positional) = decl.signature().rest_positional {
            let mut rest_items = vec![];

            for arg in call.positional_iter().skip(
                decl.signature().required_positional.len()
                    + decl.signature().optional_positional.len(),
            ) {
                let result = eval_expression(engine_state, caller_stack, arg)?;
                rest_items.push(result);
            }

            let span = if let Some(rest_item) = rest_items.first() {
                rest_item.span()?
            } else {
                call.head
            };

            callee_stack.add_var(
                rest_positional
                    .var_id
                    .expect("Internal error: rest positional parameter lacks var_id"),
                Value::List {
                    vals: rest_items,
                    span,
                },
            )
        }

        for named in decl.signature().named {
            if let Some(var_id) = named.var_id {
                let mut found = false;
                for call_named in call.named_iter() {
                    if call_named.0.item == named.long {
                        if let Some(arg) = &call_named.2 {
                            let result = eval_expression(engine_state, caller_stack, arg)?;

                            callee_stack.add_var(var_id, result);
                        } else if let Some(arg) = &named.default_value {
                            let result = eval_expression(engine_state, caller_stack, arg)?;

                            callee_stack.add_var(var_id, result);
                        } else {
                            callee_stack.add_var(var_id, Value::boolean(true, call.head))
                        }
                        found = true;
                    }
                }

                if !found {
                    if named.arg.is_none() {
                        callee_stack.add_var(var_id, Value::boolean(false, call.head))
                    } else if let Some(arg) = &named.default_value {
                        let result = eval_expression(engine_state, caller_stack, arg)?;

                        callee_stack.add_var(var_id, result);
                    } else {
                        callee_stack.add_var(var_id, Value::Nothing { span: call.head })
                    }
                }
            }
        }

        let result = eval_block_with_early_return(
            engine_state,
            &mut callee_stack,
            block,
            input,
            call.redirect_stdout,
            call.redirect_stderr,
        );

        if block.redirect_env {
            redirect_env(engine_state, caller_stack, &callee_stack);
        }

        result
    } else {
        // We pass caller_stack here with the knowledge that internal commands
        // are going to be specifically looking for global state in the stack
        // rather than any local state.
        decl.run(engine_state, caller_stack, call, input)
    }
}

/// Redirect the environment from callee to the caller.
pub fn redirect_env(engine_state: &EngineState, caller_stack: &mut Stack, callee_stack: &Stack) {
    // Grab all environment variables from the callee
    let caller_env_vars = caller_stack.get_env_var_names(engine_state);

    // remove env vars that are present in the caller but not in the callee
    // (the callee hid them)
    for var in caller_env_vars.iter() {
        if !callee_stack.has_env_var(engine_state, var) {
            caller_stack.remove_env_var(engine_state, var);
        }
    }

    // add new env vars from callee to caller
    for (var, value) in callee_stack.get_stack_env_vars() {
        caller_stack.add_env_var(var, value);
    }
}

/// Eval external expression
///
/// It returns PipelineData with a boolean flag, indicate that if the external runs to failed.
#[allow(clippy::too_many_arguments)]
fn eval_external(
    engine_state: &EngineState,
    stack: &mut Stack,
    head: &Expression,
    args: &[Expression],
    input: PipelineData,
    redirect_stdout: bool,
    redirect_stderr: bool,
    is_subexpression: bool,
) -> Result<PipelineData, ShellError> {
    let decl_id = engine_state
        .find_decl("run-external".as_bytes(), &[])
        .ok_or(ShellError::ExternalNotSupported(head.span))?;

    let command = engine_state.get_decl(decl_id);

    let mut call = Call::new(head.span);

    call.add_positional(head.clone());

    for arg in args {
        call.add_positional(arg.clone())
    }

    if redirect_stdout {
        call.add_named((
            Spanned {
                item: "redirect-stdout".into(),
                span: head.span,
            },
            None,
            None,
        ))
    }

    if redirect_stderr {
        call.add_named((
            Spanned {
                item: "redirect-stderr".into(),
                span: head.span,
            },
            None,
            None,
        ))
    }

    if is_subexpression {
        call.add_named((
            Spanned {
                item: "trim-end-newline".into(),
                span: head.span,
            },
            None,
            None,
        ))
    }

    command.run(engine_state, stack, &call, input)
}

pub fn eval_expression(
    engine_state: &EngineState,
    stack: &mut Stack,
    expr: &Expression,
) -> Result<Value, ShellError> {
    match &expr.expr {
        Expr::Bool(b) => Ok(Value::boolean(*b, expr.span)),
        Expr::Int(i) => Ok(Value::int(*i, expr.span)),
        Expr::Float(f) => Ok(Value::float(*f, expr.span)),
        Expr::Binary(b) => Ok(Value::Binary {
            val: b.clone(),
            span: expr.span,
        }),
        Expr::ValueWithUnit(e, unit) => match eval_expression(engine_state, stack, e)? {
            Value::Int { val, .. } => Ok(compute(val, unit.item, unit.span)),
            x => Err(ShellError::CantConvert(
                "unit value".into(),
                x.get_type().to_string(),
                e.span,
                None,
            )),
        },
        Expr::Range(from, next, to, operator) => {
            let from = if let Some(f) = from {
                eval_expression(engine_state, stack, f)?
            } else {
                Value::Nothing { span: expr.span }
            };

            let next = if let Some(s) = next {
                eval_expression(engine_state, stack, s)?
            } else {
                Value::Nothing { span: expr.span }
            };

            let to = if let Some(t) = to {
                eval_expression(engine_state, stack, t)?
            } else {
                Value::Nothing { span: expr.span }
            };

            Ok(Value::Range {
                val: Box::new(Range::new(expr.span, from, next, to, operator)?),
                span: expr.span,
            })
        }
        Expr::Var(var_id) => eval_variable(engine_state, stack, *var_id, expr.span),
        Expr::VarDecl(_) => Ok(Value::Nothing { span: expr.span }),
        Expr::CellPath(cell_path) => Ok(Value::CellPath {
            val: cell_path.clone(),
            span: expr.span,
        }),
        Expr::FullCellPath(cell_path) => {
            let value = eval_expression(engine_state, stack, &cell_path.head)?;

            value.follow_cell_path(&cell_path.tail, false, false)
        }
        Expr::ImportPattern(_) => Ok(Value::Nothing { span: expr.span }),
        Expr::Overlay(_) => {
            let name =
                String::from_utf8_lossy(engine_state.get_span_contents(&expr.span)).to_string();

            Ok(Value::String {
                val: name,
                span: expr.span,
            })
        }
        Expr::Call(call) => {
            // FIXME: protect this collect with ctrl-c
            Ok(eval_call(engine_state, stack, call, PipelineData::empty())?.into_value(call.head))
        }
        Expr::ExternalCall(head, args, is_subexpression) => {
            let span = head.span;
            // FIXME: protect this collect with ctrl-c
            Ok(eval_external(
                engine_state,
                stack,
                head,
                args,
                PipelineData::empty(),
                false,
                false,
                *is_subexpression,
            )?
            .into_value(span))
        }
        Expr::DateTime(dt) => Ok(Value::Date {
            val: *dt,
            span: expr.span,
        }),
        Expr::Operator(_) => Ok(Value::Nothing { span: expr.span }),
        Expr::UnaryNot(expr) => {
            let lhs = eval_expression(engine_state, stack, expr)?;
            match lhs {
                Value::Bool { val, .. } => Ok(Value::boolean(!val, expr.span)),
                _ => Err(ShellError::TypeMismatch("bool".to_string(), expr.span)),
            }
        }
        Expr::BinaryOp(lhs, op, rhs) => {
            let op_span = op.span;
            let op = eval_operator(op)?;

            match op {
                Operator::Boolean(boolean) => {
                    let lhs = eval_expression(engine_state, stack, lhs)?;
                    match boolean {
                        Boolean::And => {
                            if lhs.is_false() {
                                Ok(Value::boolean(false, expr.span))
                            } else {
                                let rhs = eval_expression(engine_state, stack, rhs)?;
                                lhs.and(op_span, &rhs, expr.span)
                            }
                        }
                        Boolean::Or => {
                            if lhs.is_true() {
                                Ok(Value::boolean(true, expr.span))
                            } else {
                                let rhs = eval_expression(engine_state, stack, rhs)?;
                                lhs.or(op_span, &rhs, expr.span)
                            }
                        }
                        Boolean::Xor => {
                            let rhs = eval_expression(engine_state, stack, rhs)?;
                            lhs.xor(op_span, &rhs, expr.span)
                        }
                    }
                }
                Operator::Math(math) => {
                    let lhs = eval_expression(engine_state, stack, lhs)?;
                    let rhs = eval_expression(engine_state, stack, rhs)?;

                    match math {
                        Math::Plus => lhs.add(op_span, &rhs, expr.span),
                        Math::Minus => lhs.sub(op_span, &rhs, expr.span),
                        Math::Multiply => lhs.mul(op_span, &rhs, expr.span),
                        Math::Divide => lhs.div(op_span, &rhs, expr.span),
                        Math::Append => lhs.append(op_span, &rhs, expr.span),
                        Math::Modulo => lhs.modulo(op_span, &rhs, expr.span),
                        Math::FloorDivision => lhs.floor_div(op_span, &rhs, expr.span),
                        Math::Pow => lhs.pow(op_span, &rhs, expr.span),
                    }
                }
                Operator::Comparison(comparison) => {
                    let lhs = eval_expression(engine_state, stack, lhs)?;
                    let rhs = eval_expression(engine_state, stack, rhs)?;
                    match comparison {
                        Comparison::LessThan => lhs.lt(op_span, &rhs, expr.span),
                        Comparison::LessThanOrEqual => lhs.lte(op_span, &rhs, expr.span),
                        Comparison::GreaterThan => lhs.gt(op_span, &rhs, expr.span),
                        Comparison::GreaterThanOrEqual => lhs.gte(op_span, &rhs, expr.span),
                        Comparison::Equal => lhs.eq(op_span, &rhs, expr.span),
                        Comparison::NotEqual => lhs.ne(op_span, &rhs, expr.span),
                        Comparison::In => lhs.r#in(op_span, &rhs, expr.span),
                        Comparison::NotIn => lhs.not_in(op_span, &rhs, expr.span),
                        Comparison::RegexMatch => {
                            lhs.regex_match(engine_state, op_span, &rhs, false, expr.span)
                        }
                        Comparison::NotRegexMatch => {
                            lhs.regex_match(engine_state, op_span, &rhs, true, expr.span)
                        }
                        Comparison::StartsWith => lhs.starts_with(op_span, &rhs, expr.span),
                        Comparison::EndsWith => lhs.ends_with(op_span, &rhs, expr.span),
                    }
                }
                Operator::Bits(bits) => {
                    let lhs = eval_expression(engine_state, stack, lhs)?;
                    let rhs = eval_expression(engine_state, stack, rhs)?;
                    match bits {
                        Bits::BitAnd => lhs.bit_and(op_span, &rhs, expr.span),
                        Bits::BitOr => lhs.bit_or(op_span, &rhs, expr.span),
                        Bits::BitXor => lhs.bit_xor(op_span, &rhs, expr.span),
                        Bits::ShiftLeft => lhs.bit_shl(op_span, &rhs, expr.span),
                        Bits::ShiftRight => lhs.bit_shr(op_span, &rhs, expr.span),
                    }
                }
                Operator::Assignment(assignment) => {
                    let rhs = eval_expression(engine_state, stack, rhs)?;

                    let rhs = match assignment {
                        Assignment::Assign => rhs,
                        Assignment::PlusAssign => {
                            let lhs = eval_expression(engine_state, stack, lhs)?;
                            lhs.add(op_span, &rhs, op_span)?
                        }
                        Assignment::MinusAssign => {
                            let lhs = eval_expression(engine_state, stack, lhs)?;
                            lhs.sub(op_span, &rhs, op_span)?
                        }
                        Assignment::MultiplyAssign => {
                            let lhs = eval_expression(engine_state, stack, lhs)?;
                            lhs.mul(op_span, &rhs, op_span)?
                        }
                        Assignment::DivideAssign => {
                            let lhs = eval_expression(engine_state, stack, lhs)?;
                            lhs.div(op_span, &rhs, op_span)?
                        }
                        Assignment::AppendAssign => {
                            let lhs = eval_expression(engine_state, stack, lhs)?;
                            lhs.append(op_span, &rhs, op_span)?
                        }
                    };

                    match &lhs.expr {
                        Expr::Var(var_id) | Expr::VarDecl(var_id) => {
                            let var_info = engine_state.get_var(*var_id);
                            if var_info.mutable {
                                stack.vars.insert(*var_id, rhs);
                                Ok(Value::nothing(lhs.span))
                            } else {
                                Err(ShellError::AssignmentRequiresMutableVar(lhs.span))
                            }
                        }
                        Expr::FullCellPath(cell_path) => match &cell_path.head.expr {
                            Expr::Var(var_id) | Expr::VarDecl(var_id) => {
                                // The $env variable is considered "mutable" in Nushell.
                                // As such, give it special treatment here.
                                let is_env = var_id == &ENV_VARIABLE_ID;
                                if is_env || engine_state.get_var(*var_id).mutable {
                                    let mut lhs =
                                        eval_expression(engine_state, stack, &cell_path.head)?;

                                    lhs.upsert_data_at_cell_path(&cell_path.tail, rhs)?;
                                    if is_env {
                                        // The special $env treatment: for something like $env.config.history.max_size = 2000,
                                        // get $env.config (or whichever one it is) AFTER the above mutation, and set it
                                        // as the "config" environment variable.
                                        let vardata = lhs.follow_cell_path(
                                            &[cell_path.tail[0].clone()],
                                            false,
                                            false,
                                        )?;
                                        match &cell_path.tail[0] {
                                            PathMember::String { val, .. } => {
                                                stack.add_env_var(val.to_string(), vardata);
                                            }
                                            // In case someone really wants an integer env-var
                                            PathMember::Int { val, .. } => {
                                                stack.add_env_var(val.to_string(), vardata);
                                            }
                                        }
                                    } else {
                                        stack.vars.insert(*var_id, lhs);
                                    }
                                    Ok(Value::nothing(cell_path.head.span))
                                } else {
                                    Err(ShellError::AssignmentRequiresMutableVar(lhs.span))
                                }
                            }
                            _ => Err(ShellError::AssignmentRequiresVar(lhs.span)),
                        },
                        _ => Err(ShellError::AssignmentRequiresVar(lhs.span)),
                    }
                }
            }
        }
        Expr::Subexpression(block_id) => {
            let block = engine_state.get_block(*block_id);

            // FIXME: protect this collect with ctrl-c
            Ok(
                eval_subexpression(engine_state, stack, block, PipelineData::empty())?
                    .into_value(expr.span),
            )
        }
        Expr::RowCondition(block_id) | Expr::Closure(block_id) => {
            let mut captures = HashMap::new();
            let block = engine_state.get_block(*block_id);

            for var_id in &block.captures {
                captures.insert(*var_id, stack.get_var(*var_id, expr.span)?);
            }
            Ok(Value::Closure {
                val: *block_id,
                captures,
                span: expr.span,
            })
        }
        Expr::Block(block_id) => Ok(Value::Block {
            val: *block_id,
            span: expr.span,
        }),
        Expr::List(x) => {
            let mut output = vec![];
            for expr in x {
                output.push(eval_expression(engine_state, stack, expr)?);
            }
            Ok(Value::List {
                vals: output,
                span: expr.span,
            })
        }
        Expr::Record(fields) => {
            let mut cols = vec![];
            let mut vals = vec![];
            for (col, val) in fields {
                // avoid duplicate cols.
                let col_name = eval_expression(engine_state, stack, col)?.as_string()?;
                let pos = cols.iter().position(|c| c == &col_name);
                match pos {
                    Some(index) => {
                        vals[index] = eval_expression(engine_state, stack, val)?;
                    }
                    None => {
                        cols.push(col_name);
                        vals.push(eval_expression(engine_state, stack, val)?);
                    }
                }
            }

            Ok(Value::Record {
                cols,
                vals,
                span: expr.span,
            })
        }
        Expr::Table(headers, vals) => {
            let mut output_headers = vec![];
            for expr in headers {
                output_headers.push(eval_expression(engine_state, stack, expr)?.as_string()?);
            }

            let mut output_rows = vec![];
            for val in vals {
                let mut row = vec![];
                for expr in val {
                    row.push(eval_expression(engine_state, stack, expr)?);
                }
                output_rows.push(Value::Record {
                    cols: output_headers.clone(),
                    vals: row,
                    span: expr.span,
                });
            }
            Ok(Value::List {
                vals: output_rows,
                span: expr.span,
            })
        }
        Expr::Keyword(_, _, expr) => eval_expression(engine_state, stack, expr),
        Expr::StringInterpolation(exprs) => {
            let mut parts = vec![];
            for expr in exprs {
                parts.push(eval_expression(engine_state, stack, expr)?);
            }

            let config = engine_state.get_config();

            parts
                .into_iter()
                .into_pipeline_data(None)
                .collect_string("", config)
                .map(|x| Value::String {
                    val: x,
                    span: expr.span,
                })
        }
        Expr::String(s) => Ok(Value::String {
            val: s.clone(),
            span: expr.span,
        }),
        Expr::Filepath(s) => {
            let cwd = current_dir_str(engine_state, stack)?;
            let path = expand_path_with(s, cwd);

            Ok(Value::string(path.to_string_lossy(), expr.span))
        }
        Expr::Directory(s) => {
            if s == "-" {
                Ok(Value::string("-", expr.span))
            } else {
                let cwd = current_dir_str(engine_state, stack)?;
                let path = expand_path_with(s, cwd);

                Ok(Value::string(path.to_string_lossy(), expr.span))
            }
        }
        Expr::GlobPattern(s) => {
            let cwd = current_dir_str(engine_state, stack)?;
            let path = expand_path_with(s, cwd);

            Ok(Value::string(path.to_string_lossy(), expr.span))
        }
        Expr::Signature(_) => Ok(Value::Nothing { span: expr.span }),
        Expr::Garbage => Ok(Value::Nothing { span: expr.span }),
        Expr::Nothing => Ok(Value::Nothing { span: expr.span }),
    }
}

/// Checks the expression to see if it's a internal or external call. If so, passes the input
/// into the call and gets out the result
/// Otherwise, invokes the expression
///
/// It returns PipelineData with a boolean flag, indicating if the external failed to run.
/// The boolean flag **may only be true** for external calls, for internal calls, it always to be false.
pub fn eval_expression_with_input(
    engine_state: &EngineState,
    stack: &mut Stack,
    expr: &Expression,
    mut input: PipelineData,
    redirect_stdout: bool,
    redirect_stderr: bool,
) -> Result<(PipelineData, bool), ShellError> {
    match expr {
        Expression {
            expr: Expr::Call(call),
            ..
        } => {
            if !redirect_stdout || redirect_stderr {
                // we're doing something different than the defaults
                let mut call = call.clone();
                call.redirect_stdout = redirect_stdout;
                call.redirect_stderr = redirect_stderr;
                input = eval_call(engine_state, stack, &call, input)?;
            } else {
                input = eval_call(engine_state, stack, call, input)?;
            }
        }
        Expression {
            expr: Expr::ExternalCall(head, args, is_subexpression),
            ..
        } => {
            input = eval_external(
                engine_state,
                stack,
                head,
                args,
                input,
                redirect_stdout,
                redirect_stderr,
                *is_subexpression,
            )?;
        }

        Expression {
            expr: Expr::Subexpression(block_id),
            ..
        } => {
            let block = engine_state.get_block(*block_id);

            // FIXME: protect this collect with ctrl-c
            input = eval_subexpression(engine_state, stack, block, input)?;
        }

        elem => {
            input = eval_expression(engine_state, stack, elem)?.into_pipeline_data();
        }
    };

    Ok(might_consume_external_result(input))
}

// Try to catch and detect if external command runs to failed.
fn might_consume_external_result(input: PipelineData) -> (PipelineData, bool) {
    input.is_external_failed()
}

pub fn eval_element_with_input(
    engine_state: &EngineState,
    stack: &mut Stack,
    element: &PipelineElement,
    input: PipelineData,
    redirect_stdout: bool,
    redirect_stderr: bool,
) -> Result<(PipelineData, bool), ShellError> {
    match element {
        PipelineElement::Expression(_, expr) => eval_expression_with_input(
            engine_state,
            stack,
            expr,
            input,
            redirect_stdout,
            redirect_stderr,
        ),
        PipelineElement::Redirection(span, redirection, expr) => match &expr.expr {
            Expr::String(_) => {
                let input = match (redirection, input) {
                    (
                        Redirection::Stderr,
                        PipelineData::ExternalStream {
                            stderr,
                            exit_code,
                            span,
                            metadata,
                            trim_end_newline,
                            ..
                        },
                    ) => PipelineData::ExternalStream {
                        stdout: stderr,
                        stderr: None,
                        exit_code,
                        span,
                        metadata,
                        trim_end_newline,
                    },
                    (
                        Redirection::StdoutAndStderr,
                        PipelineData::ExternalStream {
                            stdout,
                            stderr,
                            exit_code,
                            span,
                            metadata,
                            trim_end_newline,
                        },
                    ) => match (stdout, stderr) {
                        (Some(stdout), Some(stderr)) => PipelineData::ExternalStream {
                            stdout: Some(stdout.chain(stderr)),
                            stderr: None,
                            exit_code,
                            span,
                            metadata,
                            trim_end_newline,
                        },
                        (None, Some(stderr)) => PipelineData::ExternalStream {
                            stdout: Some(stderr),
                            stderr: None,
                            exit_code,
                            span,
                            metadata,
                            trim_end_newline,
                        },
                        (Some(stdout), None) => PipelineData::ExternalStream {
                            stdout: Some(stdout),
                            stderr: None,
                            exit_code,
                            span,
                            metadata,
                            trim_end_newline,
                        },
                        (None, None) => PipelineData::ExternalStream {
                            stdout: None,
                            stderr: None,
                            exit_code,
                            span,
                            metadata,
                            trim_end_newline,
                        },
                    },
                    (_, input) => input,
                };

                if let Some(save_command) = engine_state.find_decl(b"save", &[]) {
                    eval_call(
                        engine_state,
                        stack,
                        &Call {
                            decl_id: save_command,
                            head: *span,
                            arguments: vec![
                                Argument::Positional(expr.clone()),
                                Argument::Named((
                                    Spanned {
                                        item: "raw".into(),
                                        span: *span,
                                    },
                                    None,
                                    None,
                                )),
                                Argument::Named((
                                    Spanned {
                                        item: "force".into(),
                                        span: *span,
                                    },
                                    None,
                                    None,
                                )),
                            ],
                            redirect_stdout: false,
                            redirect_stderr: false,
                            parser_info: vec![],
                        },
                        input,
                    )
                    .map(|x| (x, false))
                } else {
                    Err(ShellError::CommandNotFound(*span))
                }
            }
            _ => Err(ShellError::CommandNotFound(*span)),
        },
        PipelineElement::And(_, expr) => eval_expression_with_input(
            engine_state,
            stack,
            expr,
            input,
            redirect_stdout,
            redirect_stderr,
        ),
        PipelineElement::Or(_, expr) => eval_expression_with_input(
            engine_state,
            stack,
            expr,
            input,
            redirect_stdout,
            redirect_stderr,
        ),
    }
}

pub fn eval_block_with_early_return(
    engine_state: &EngineState,
    stack: &mut Stack,
    block: &Block,
    input: PipelineData,
    redirect_stdout: bool,
    redirect_stderr: bool,
) -> Result<PipelineData, ShellError> {
    match eval_block(
        engine_state,
        stack,
        block,
        input,
        redirect_stdout,
        redirect_stderr,
    ) {
        Err(ShellError::Return(_, value)) => Ok(PipelineData::Value(*value, None)),
        x => x,
    }
}

pub fn eval_block(
    engine_state: &EngineState,
    stack: &mut Stack,
    block: &Block,
    mut input: PipelineData,
    redirect_stdout: bool,
    redirect_stderr: bool,
) -> Result<PipelineData, ShellError> {
    // if Block contains recursion, make sure we don't recurse too deeply (to avoid stack overflow)
    if let Some(recursive) = block.recursive {
        // picked 50 arbitrarily, should work on all architectures
        const RECURSION_LIMIT: u64 = 50;
        if recursive {
            if *stack.recursion_count >= RECURSION_LIMIT {
                stack.recursion_count = Box::new(0);
                return Err(ShellError::RecursionLimitReached {
                    recursion_limit: RECURSION_LIMIT,
                    span: block.span,
                });
            }
            *stack.recursion_count += 1;
        }
    }
    let num_pipelines = block.len();
    for (pipeline_idx, pipeline) in block.pipelines.iter().enumerate() {
        let mut i = 0;

        while i < pipeline.elements.len() {
            let redirect_stderr = redirect_stderr
                || ((i < pipeline.elements.len() - 1)
                    && (matches!(
                        pipeline.elements[i + 1],
                        PipelineElement::Redirection(_, Redirection::Stderr, _)
                            | PipelineElement::Redirection(_, Redirection::StdoutAndStderr, _)
                    )));

            // if eval internal command failed, it can just make early return with `Err(ShellError)`.
            let eval_result = eval_element_with_input(
                engine_state,
                stack,
                &pipeline.elements[i],
                input,
                redirect_stdout
                    || (i != pipeline.elements.len() - 1)
                        && (matches!(
                            pipeline.elements[i + 1],
                            PipelineElement::Redirection(_, Redirection::Stdout, _)
                                | PipelineElement::Redirection(_, Redirection::StdoutAndStderr, _)
                                | PipelineElement::Expression(..)
                        )),
                redirect_stderr,
            );

            match (eval_result, redirect_stderr) {
                (Ok((pipeline_data, _)), true) => {
                    input = pipeline_data;

                    // external command may runs to failed
                    // make early return so remaining commands will not be executed.
                    // don't return `Err(ShellError)`, so nushell wouldn't show extra error message.
                }
                (Err(error), true) => input = PipelineData::Value(Value::Error { error }, None),
                (output, false) => {
                    let output = output?;
                    input = output.0;
                    // external command may runs to failed
                    // make early return so remaining commands will not be executed.
                    // don't return `Err(ShellError)`, so nushell wouldn't show extra error message.
                    if output.1 {
                        return Ok(input);
                    }
                }
            }

            i += 1;
        }

        if pipeline_idx < (num_pipelines) - 1 {
            match input {
                PipelineData::Value(Value::Nothing { .. }, ..) => {}
                PipelineData::ExternalStream {
                    ref mut exit_code, ..
                } => {
                    let exit_code = exit_code.take();

                    // Drain the input to the screen via tabular output
                    let config = engine_state.get_config();

                    match engine_state.find_decl("table".as_bytes(), &[]) {
                        Some(decl_id) => {
                            let table = engine_state.get_decl(decl_id).run(
                                engine_state,
                                stack,
                                &Call::new(Span::new(0, 0)),
                                input,
                            )?;

                            print_or_return(table, config)?;
                        }
                        None => {
                            print_or_return(input, config)?;
                        }
                    };

                    if let Some(exit_code) = exit_code {
                        let mut v: Vec<_> = exit_code.collect();

                        if let Some(v) = v.pop() {
                            stack.add_env_var("LAST_EXIT_CODE".into(), v);
                        }
                    }
                }
                _ => {
                    // Drain the input to the screen via tabular output
                    let config = engine_state.get_config();

                    match engine_state.find_decl("table".as_bytes(), &[]) {
                        Some(decl_id) => {
                            let table = engine_state.get_decl(decl_id);

                            if let Some(block_id) = table.get_block_id() {
                                let block = engine_state.get_block(block_id);
                                eval_block(
                                    engine_state,
                                    stack,
                                    block,
                                    input,
                                    redirect_stdout,
                                    redirect_stderr,
                                )?;
                            } else {
                                let table = table.run(
                                    engine_state,
                                    stack,
                                    &Call::new(Span::new(0, 0)),
                                    input,
                                )?;

                                print_or_return(table, config)?;
                            }
                        }
                        None => {
                            print_or_return(input, config)?;
                        }
                    };
                }
            }

            input = PipelineData::empty()
        }
    }

    Ok(input)
}

fn print_or_return(pipeline_data: PipelineData, config: &Config) -> Result<(), ShellError> {
    for item in pipeline_data {
        if let Value::Error { error } = item {
            return Err(error);
        }

        let mut out = item.into_string("\n", config);
        out.push('\n');

        stdout_write_all_and_flush(out)?;
    }

    Ok(())
}

pub fn eval_subexpression(
    engine_state: &EngineState,
    stack: &mut Stack,
    block: &Block,
    mut input: PipelineData,
) -> Result<PipelineData, ShellError> {
    for pipeline in block.pipelines.iter() {
        for expr in pipeline.elements.iter() {
            input = eval_element_with_input(engine_state, stack, expr, input, true, false)?.0
        }
    }

    Ok(input)
}

pub fn eval_variable(
    engine_state: &EngineState,
    stack: &Stack,
    var_id: VarId,
    span: Span,
) -> Result<Value, ShellError> {
    match var_id {
        nu_protocol::NU_VARIABLE_ID => {
            // $nu
            let mut output_cols = vec![];
            let mut output_vals = vec![];

            if let Some(path) = engine_state.get_config_path("config-path") {
                output_cols.push("config-path".into());
                output_vals.push(Value::String {
                    val: path.to_string_lossy().to_string(),
                    span,
                });
            }

            if let Some(path) = engine_state.get_config_path("env-path") {
                output_cols.push("env-path".into());
                output_vals.push(Value::String {
                    val: path.to_string_lossy().to_string(),
                    span,
                });
            }

            if let Some(mut config_path) = nu_path::config_dir() {
                config_path.push("nushell");
                let mut env_config_path = config_path.clone();
                let mut loginshell_path = config_path.clone();

                let mut history_path = config_path.clone();

                match engine_state.config.history_file_format {
                    HistoryFileFormat::Sqlite => {
                        history_path.push("history.sqlite3");
                    }
                    HistoryFileFormat::PlainText => {
                        history_path.push("history.txt");
                    }
                }
                // let mut history_path = config_files::get_history_path(); // todo: this should use the get_history_path method but idk where to put that function

                output_cols.push("history-path".into());
                output_vals.push(Value::String {
                    val: history_path.to_string_lossy().to_string(),
                    span,
                });

                if engine_state.get_config_path("config-path").is_none() {
                    config_path.push("config.nu");

                    output_cols.push("config-path".into());
                    output_vals.push(Value::String {
                        val: config_path.to_string_lossy().to_string(),
                        span,
                    });
                }

                if engine_state.get_config_path("env-path").is_none() {
                    env_config_path.push("env.nu");

                    output_cols.push("env-path".into());
                    output_vals.push(Value::String {
                        val: env_config_path.to_string_lossy().to_string(),
                        span,
                    });
                }

                loginshell_path.push("login.nu");

                output_cols.push("loginshell-path".into());
                output_vals.push(Value::String {
                    val: loginshell_path.to_string_lossy().to_string(),
                    span,
                });
            }

            #[cfg(feature = "plugin")]
            if let Some(path) = &engine_state.plugin_signatures {
                if let Some(path_str) = path.to_str() {
                    output_cols.push("plugin-path".into());
                    output_vals.push(Value::String {
                        val: path_str.into(),
                        span,
                    });
                }
            }

            output_cols.push("scope".into());
            output_vals.push(create_scope(engine_state, stack, span)?);

            if let Some(home_path) = nu_path::home_dir() {
                if let Some(home_path_str) = home_path.to_str() {
                    output_cols.push("home-path".into());
                    output_vals.push(Value::String {
                        val: home_path_str.into(),
                        span,
                    })
                }
            }

            let temp = std::env::temp_dir();
            if let Some(temp_path) = temp.to_str() {
                output_cols.push("temp-path".into());
                output_vals.push(Value::String {
                    val: temp_path.into(),
                    span,
                })
            }

            let pid = std::process::id();
            output_cols.push("pid".into());
            output_vals.push(Value::int(pid as i64, span));

            let sys = sysinfo::System::new();
            let ver = match sys.kernel_version() {
                Some(v) => v,
                None => "unknown".into(),
            };

            let os_record = Value::Record {
                cols: vec![
                    "name".into(),
                    "arch".into(),
                    "family".into(),
                    "kernel_version".into(),
                ],
                vals: vec![
                    Value::string(std::env::consts::OS, span),
                    Value::string(std::env::consts::ARCH, span),
                    Value::string(std::env::consts::FAMILY, span),
                    Value::string(ver, span),
                ],
                span,
            };
            output_cols.push("os-info".into());
            output_vals.push(os_record);

            Ok(Value::Record {
                cols: output_cols,
                vals: output_vals,
                span,
            })
        }
        ENV_VARIABLE_ID => {
            let env_vars = stack.get_env_vars(engine_state);
            let env_columns = env_vars.keys();
            let env_values = env_vars.values();

            let mut pairs = env_columns
                .map(|x| x.to_string())
                .zip(env_values.cloned())
                .collect::<Vec<(String, Value)>>();

            pairs.sort_by(|a, b| a.0.cmp(&b.0));

            let (env_columns, env_values) = pairs.into_iter().unzip();

            Ok(Value::Record {
                cols: env_columns,
                vals: env_values,
                span,
            })
        }
        var_id => stack.get_var(var_id, span),
    }
}

fn compute(size: i64, unit: Unit, span: Span) -> Value {
    match unit {
        Unit::Byte => Value::Filesize { val: size, span },
        Unit::Kilobyte => Value::Filesize {
            val: size * 1000,
            span,
        },
        Unit::Megabyte => Value::Filesize {
            val: size * 1000 * 1000,
            span,
        },
        Unit::Gigabyte => Value::Filesize {
            val: size * 1000 * 1000 * 1000,
            span,
        },
        Unit::Terabyte => Value::Filesize {
            val: size * 1000 * 1000 * 1000 * 1000,
            span,
        },
        Unit::Petabyte => Value::Filesize {
            val: size * 1000 * 1000 * 1000 * 1000 * 1000,
            span,
        },
        Unit::Exabyte => Value::Filesize {
            val: size * 1000 * 1000 * 1000 * 1000 * 1000 * 1000,
            span,
        },
        Unit::Zettabyte => Value::Filesize {
            val: size * 1000 * 1000 * 1000 * 1000 * 1000 * 1000 * 1000,
            span,
        },

        Unit::Kibibyte => Value::Filesize {
            val: size * 1024,
            span,
        },
        Unit::Mebibyte => Value::Filesize {
            val: size * 1024 * 1024,
            span,
        },
        Unit::Gibibyte => Value::Filesize {
            val: size * 1024 * 1024 * 1024,
            span,
        },
        Unit::Tebibyte => Value::Filesize {
            val: size * 1024 * 1024 * 1024 * 1024,
            span,
        },
        Unit::Pebibyte => Value::Filesize {
            val: size * 1024 * 1024 * 1024 * 1024 * 1024,
            span,
        },
        Unit::Exbibyte => Value::Filesize {
            val: size * 1024 * 1024 * 1024 * 1024 * 1024 * 1024,
            span,
        },
        Unit::Zebibyte => Value::Filesize {
            val: size * 1024 * 1024 * 1024 * 1024 * 1024 * 1024 * 1024,
            span,
        },

        Unit::Nanosecond => Value::Duration { val: size, span },
        Unit::Microsecond => Value::Duration {
            val: size * 1000,
            span,
        },
        Unit::Millisecond => Value::Duration {
            val: size * 1000 * 1000,
            span,
        },
        Unit::Second => Value::Duration {
            val: size * 1000 * 1000 * 1000,
            span,
        },
        Unit::Minute => match size.checked_mul(1000 * 1000 * 1000 * 60) {
            Some(val) => Value::Duration { val, span },
            None => Value::Error {
                error: ShellError::GenericError(
                    "duration too large".into(),
                    "duration too large".into(),
                    Some(span),
                    None,
                    Vec::new(),
                ),
            },
        },
        Unit::Hour => match size.checked_mul(1000 * 1000 * 1000 * 60 * 60) {
            Some(val) => Value::Duration { val, span },
            None => Value::Error {
                error: ShellError::GenericError(
                    "duration too large".into(),
                    "duration too large".into(),
                    Some(span),
                    None,
                    Vec::new(),
                ),
            },
        },
        Unit::Day => match size.checked_mul(1000 * 1000 * 1000 * 60 * 60 * 24) {
            Some(val) => Value::Duration { val, span },
            None => Value::Error {
                error: ShellError::GenericError(
                    "duration too large".into(),
                    "duration too large".into(),
                    Some(span),
                    None,
                    Vec::new(),
                ),
            },
        },
        Unit::Week => match size.checked_mul(1000 * 1000 * 1000 * 60 * 60 * 24 * 7) {
            Some(val) => Value::Duration { val, span },
            None => Value::Error {
                error: ShellError::GenericError(
                    "duration too large".into(),
                    "duration too large".into(),
                    Some(span),
                    None,
                    Vec::new(),
                ),
            },
        },
    }
}
