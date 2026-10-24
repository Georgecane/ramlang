//! Parser and lowering for the initial dependency-free Ramlang interpreter subset.
//! Integer source types are currently normalized to i64 by this reference interpreter;
//! this is not yet a replacement for the typed, target-aware compiler front end.

use std::collections::HashMap;

use crate::{Function, FunctionId, Instruction, Program, Register, Type, VmError};

#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenKind { Ident, Number, String, Symbol(String), Eof }

#[derive(Debug, Clone)]
struct Token { kind: TokenKind, text: String, line: usize, column: usize }

struct Lexer<'a> { source: &'a [u8], index: usize, line: usize, column: usize }

impl<'a> Lexer<'a> {
    fn new(source: &'a str) -> Self { Self { source: source.as_bytes(), index: 0, line: 1, column: 1 } }
    fn peek(&self, offset: usize) -> Option<u8> { self.source.get(self.index + offset).copied() }
    fn advance(&mut self) -> Option<u8> {
        let byte = self.peek(0)?;
        self.index += 1;
        if byte == b'\n' { self.line += 1; self.column = 1; } else { self.column += 1; }
        Some(byte)
    }
    fn lex(mut self) -> Result<Vec<Token>, VmError> {
        let mut tokens = Vec::new();
        while let Some(byte) = self.peek(0) {
            if byte.is_ascii_whitespace() { self.advance(); continue; }
            if byte == b'/' && self.peek(1) == Some(b'/') {
                while self.peek(0).is_some_and(|c| c != b'\n') { self.advance(); }
                continue;
            }
            if byte == b'/' && self.peek(1) == Some(b'*') {
                let line = self.line; let column = self.column;
                self.advance(); self.advance();
                let mut closed = false;
                while self.peek(0).is_some() {
                    if self.peek(0) == Some(b'*') && self.peek(1) == Some(b'/') {
                        self.advance(); self.advance(); closed = true; break;
                    }
                    self.advance();
                }
                if !closed { return Err(VmError::new(format!("{line}:{column}: unterminated block comment"))); }
                continue;
            }
            let line = self.line; let column = self.column;
            if byte.is_ascii_alphabetic() || byte == b'_' {
                let start = self.index;
                self.advance();
                while self.peek(0).is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_') { self.advance(); }
                let text = String::from_utf8(self.source[start..self.index].to_vec())
                    .map_err(|_| VmError::new(format!("{line}:{column}: invalid identifier encoding")))?;
                tokens.push(Token { kind: TokenKind::Ident, text, line, column });
                continue;
            }
            if byte.is_ascii_digit() {
                let start = self.index;
                self.advance();
                while self.peek(0).is_some_and(|c| c.is_ascii_digit()) { self.advance(); }
                let text = String::from_utf8(self.source[start..self.index].to_vec())
                    .map_err(|_| VmError::new(format!("{line}:{column}: invalid integer literal")))?;
                tokens.push(Token { kind: TokenKind::Number, text, line, column });
                continue;
            }
            if byte == b'"' {
                self.advance();
                let mut bytes = Vec::new();
                let mut closed = false;
                while let Some(next) = self.peek(0) {
                    match next {
                        b'"' => { self.advance(); closed = true; break; }
                        b'\n' => return Err(VmError::new(format!("{line}:{column}: unescaped newline in string literal"))),
                        b'\\' => {
                            self.advance();
                            let escaped = self.peek(0).ok_or_else(|| VmError::new(format!("{line}:{column}: unterminated string escape")))?;
                            self.advance();
                            match escaped {
                                b'n' => bytes.push(b'\n'),
                                b'r' => bytes.push(b'\r'),
                                b't' => bytes.push(b'\t'),
                                b'0' => bytes.push(0),
                                b'\\' => bytes.push(b'\\'),
                                b'"' => bytes.push(b'"'),
                                _ => return Err(VmError::new(format!("{line}:{column}: unsupported string escape"))),
                            }
                        }
                        _ => bytes.push(self.advance().expect("peeked byte exists")),
                    }
                }
                if !closed { return Err(VmError::new(format!("{line}:{column}: unterminated string literal"))); }
                let text = String::from_utf8(bytes)
                    .map_err(|_| VmError::new(format!("{line}:{column}: string literal is not valid UTF-8")))?;
                tokens.push(Token { kind: TokenKind::String, text, line, column });
                continue;
            }
            let two = match (byte, self.peek(1)) {
                (b'-', Some(b'>')) => Some("->"),
                (b'=', Some(b'=')) => Some("=="),
                (b'!', Some(b'=')) => Some("!="),
                (b'<', Some(b'=')) => Some("<="),
                (b'>', Some(b'=')) => Some(">="),
                (b'&', Some(b'&')) => Some("&&"),
                (b'|', Some(b'|')) => Some("||"),
                _ => None,
            };
            if let Some(symbol) = two {
                self.advance(); self.advance();
                tokens.push(Token { kind: TokenKind::Symbol(symbol.to_owned()), text: symbol.to_owned(), line, column });
                continue;
            }
            if b"(){}:;,=+-*/<>!".contains(&byte) {
                self.advance();
                let symbol = char::from(byte).to_string();
                tokens.push(Token { kind: TokenKind::Symbol(symbol.clone()), text: symbol.clone(), line, column });
                continue;
            }
            return Err(VmError::new(format!("{line}:{column}: unsupported character '{}'", char::from(byte))));
        }
        tokens.push(Token { kind: TokenKind::Eof, text: String::new(), line: self.line, column: self.column });
        Ok(tokens)
    }
}

#[derive(Debug, Clone)]
struct FunctionAst { name: String, params: Vec<(String, Type)>, result: Type, body: Stmt }
#[derive(Debug, Clone)]
enum Stmt {
    Block(Vec<Stmt>),
    Let { name: String, declared: Option<Type>, mutable: bool, value: Expr },
    Assign { name: String, value: Expr },
    Return(Option<Expr>),
    Expression(Expr),
    If { condition: Expr, then_branch: Box<Stmt>, else_branch: Option<Box<Stmt>> },
    While { condition: Expr, body: Box<Stmt> },
}
#[derive(Debug, Clone)]
enum Expr {
    Int(i64),
    Bool(bool),
    String(String),
    Variable(String),
    Unary { op: String, value: Box<Expr> },
    Binary { left: Box<Expr>, op: String, right: Box<Expr> },
    Call { name: String, args: Vec<Expr> },
}
struct Parser { tokens: Vec<Token>, index: usize }

impl Parser {
    fn new(tokens: Vec<Token>) -> Self { Self { tokens, index: 0 } }
    fn current(&self) -> &Token { &self.tokens[self.index] }
    fn error(&self, message: impl AsRef<str>) -> VmError {
        VmError::new(format!("{}:{}: {}", self.current().line, self.current().column, message.as_ref()))
    }
    fn check_symbol(&self, symbol: &str) -> bool { self.current().kind == TokenKind::Symbol(symbol.to_owned()) }
    fn match_symbol(&mut self, symbol: &str) -> bool {
        if self.check_symbol(symbol) { self.index += 1; true } else { false }
    }
    fn match_keyword(&mut self, keyword: &str) -> bool {
        if self.current().kind == TokenKind::Ident && self.current().text == keyword { self.index += 1; true } else { false }
    }
    fn expect_symbol(&mut self, symbol: &str) -> Result<(), VmError> {
        if self.match_symbol(symbol) { Ok(()) } else { Err(self.error(format!("expected '{symbol}'"))) }
    }
    fn expect_ident(&mut self, what: &str) -> Result<String, VmError> {
        if self.current().kind == TokenKind::Ident {
            let name = self.current().text.clone(); self.index += 1; Ok(name)
        } else { Err(self.error(format!("expected {what}"))) }
    }
    fn parse_type(&mut self) -> Result<Type, VmError> {
        let name = self.expect_ident("a type name")?;
        match name.as_str() {
            "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" | "u64" => Ok(Type::I64),
            "bool" => Ok(Type::Bool),
            "str" => Ok(Type::Str),
            "void" | "unit" => Ok(Type::Unit),
            _ => Err(self.error(format!("the reference runner does not yet support type '{name}'"))),
        }
    }
    fn parse_program(&mut self) -> Result<Vec<FunctionAst>, VmError> {
        let mut functions = Vec::new();
        while self.current().kind != TokenKind::Eof {
            if !self.match_keyword("fn") { return Err(self.error("expected a function declaration beginning with 'fn'")); }
            let name = self.expect_ident("a function name")?;
            self.expect_symbol("(")?;
            let mut params = Vec::new();
            if !self.check_symbol(")") {
                loop {
                    let param_name = self.expect_ident("a parameter name")?;
                    self.expect_symbol(":")?;
                    let param_type = self.parse_type()?;
                    if param_type == Type::Unit { return Err(self.error("parameters cannot have type void")); }
                    params.push((param_name, param_type));
                    if !self.match_symbol(",") { break; }
                }
            }
            self.expect_symbol(")")?;
            let result = if self.match_symbol("->") { self.parse_type()? } else { Type::Unit };
            let body = self.parse_block()?;
            functions.push(FunctionAst { name, params, result, body });
        }
        Ok(functions)
    }
    fn parse_block(&mut self) -> Result<Stmt, VmError> {
        self.expect_symbol("{")?;
        let mut statements = Vec::new();
        while !self.check_symbol("}") && self.current().kind != TokenKind::Eof {
            statements.push(self.parse_statement()?);
        }
        self.expect_symbol("}")?;
        Ok(Stmt::Block(statements))
    }
    fn parse_statement(&mut self) -> Result<Stmt, VmError> {
        if self.match_keyword("let") {
            let mutable = self.match_keyword("mut");
            let name = self.expect_ident("a variable name")?;
            let declared = if self.match_symbol(":") { Some(self.parse_type()?) } else { None };
            self.expect_symbol("=")?;
            let value = self.parse_expression()?;
            self.expect_symbol(";")?;
            return Ok(Stmt::Let { name, declared, mutable, value });
        }
        if self.match_keyword("return") {
            let value = if self.check_symbol(";") { None } else { Some(self.parse_expression()?) };
            self.expect_symbol(";")?;
            return Ok(Stmt::Return(value));
        }
        if self.match_keyword("if") {
            let condition = self.parse_parenthesized_or_plain_expression()?;
            let then_branch = Box::new(self.parse_block()?);
            let else_branch = if self.match_keyword("else") {
                Some(Box::new(if self.current().kind == TokenKind::Ident && self.current().text == "if" {
                    self.index += 1;
                    let nested_condition = self.parse_parenthesized_or_plain_expression()?;
                    let nested_then = Box::new(self.parse_block()?);
                    let nested_else = if self.match_keyword("else") { Some(Box::new(self.parse_block()?)) } else { None };
                    Stmt::If { condition: nested_condition, then_branch: nested_then, else_branch: nested_else }
                } else { self.parse_block()? }))
            } else { None };
            return Ok(Stmt::If { condition, then_branch, else_branch });
        }
        if self.match_keyword("while") {
            let condition = self.parse_parenthesized_or_plain_expression()?;
            let body = Box::new(self.parse_block()?);
            return Ok(Stmt::While { condition, body });
        }
        if self.check_symbol("{") { return self.parse_block(); }
        if self.current().kind == TokenKind::Ident
            && self.tokens.get(self.index + 1).is_some_and(|t| t.kind == TokenKind::Symbol("=".to_owned()))
        {
            let name = self.current().text.clone(); self.index += 2;
            let value = self.parse_expression()?;
            self.expect_symbol(";")?;
            return Ok(Stmt::Assign { name, value });
        }
        let expression = self.parse_expression()?;
        self.expect_symbol(";")?;
        Ok(Stmt::Expression(expression))
    }
    fn parse_parenthesized_or_plain_expression(&mut self) -> Result<Expr, VmError> {
        if self.match_symbol("(") {
            let expression = self.parse_expression()?;
            self.expect_symbol(")")?;
            Ok(expression)
        } else { self.parse_expression() }
    }
    fn parse_expression(&mut self) -> Result<Expr, VmError> { self.parse_or() }
    fn parse_or(&mut self) -> Result<Expr, VmError> {
        let mut expression = self.parse_and()?;
        while self.match_symbol("||") { expression = Expr::Binary { left: Box::new(expression), op: "||".into(), right: Box::new(self.parse_and()?) }; }
        Ok(expression)
    }
    fn parse_and(&mut self) -> Result<Expr, VmError> {
        let mut expression = self.parse_equality()?;
        while self.match_symbol("&&") { expression = Expr::Binary { left: Box::new(expression), op: "&&".into(), right: Box::new(self.parse_equality()?) }; }
        Ok(expression)
    }
    fn parse_equality(&mut self) -> Result<Expr, VmError> {
        let mut expression = self.parse_comparison()?;
        while self.check_symbol("==") || self.check_symbol("!=") {
            let op = self.current().text.clone(); self.index += 1;
            expression = Expr::Binary { left: Box::new(expression), op, right: Box::new(self.parse_comparison()?) };
        }
        Ok(expression)
    }
    fn parse_comparison(&mut self) -> Result<Expr, VmError> {
        let mut expression = self.parse_term()?;
        while ["<", "<=", ">", ">="].iter().any(|op| self.check_symbol(op)) {
            let op = self.current().text.clone(); self.index += 1;
            expression = Expr::Binary { left: Box::new(expression), op, right: Box::new(self.parse_term()?) };
        }
        Ok(expression)
    }
    fn parse_term(&mut self) -> Result<Expr, VmError> {
        let mut expression = self.parse_factor()?;
        while self.check_symbol("+") || self.check_symbol("-") {
            let op = self.current().text.clone(); self.index += 1;
            expression = Expr::Binary { left: Box::new(expression), op, right: Box::new(self.parse_factor()?) };
        }
        Ok(expression)
    }
    fn parse_factor(&mut self) -> Result<Expr, VmError> {
        let mut expression = self.parse_unary()?;
        while self.check_symbol("*") || self.check_symbol("/") {
            let op = self.current().text.clone(); self.index += 1;
            expression = Expr::Binary { left: Box::new(expression), op, right: Box::new(self.parse_unary()?) };
        }
        Ok(expression)
    }
    fn parse_unary(&mut self) -> Result<Expr, VmError> {
        if self.check_symbol("-") || self.check_symbol("!") {
            let op = self.current().text.clone(); self.index += 1;
            return Ok(Expr::Unary { op, value: Box::new(self.parse_unary()?) });
        }
        self.parse_primary()
    }
    fn parse_primary(&mut self) -> Result<Expr, VmError> {
        if matches!(self.current().kind, TokenKind::String) {
            let token = self.current().clone();
            self.index += 1;
            return Ok(Expr::String(token.text));
        }
        if matches!(self.current().kind, TokenKind::Number) {
            let token = self.current().clone(); self.index += 1;
            let value = token.text.parse::<i64>().map_err(|_| VmError::new(format!("{}:{}: integer literal is outside the i64 range", token.line, token.column)))?;
            return Ok(Expr::Int(value));
        }
        if self.match_keyword("true") { return Ok(Expr::Bool(true)); }
        if self.match_keyword("false") { return Ok(Expr::Bool(false)); }
        if self.match_symbol("(") {
            let expression = self.parse_expression()?;
            self.expect_symbol(")")?;
            return Ok(expression);
        }
        if self.current().kind == TokenKind::Ident {
            let name = self.current().text.clone(); self.index += 1;
            if self.match_symbol("(") {
                let mut args = Vec::new();
                if !self.check_symbol(")") {
                    loop { args.push(self.parse_expression()?); if !self.match_symbol(",") { break; } }
                }
                self.expect_symbol(")")?;
                return Ok(Expr::Call { name, args });
            }
            return Ok(Expr::Variable(name));
        }
        Err(self.error("expected a literal, variable, function call, or parenthesized expression"))
    }
}

#[derive(Clone)]
struct Signature { id: FunctionId, params: Vec<Type>, result: Type }
#[derive(Clone)]
struct Binding { register: Register, ty: Type, mutable: bool }

pub fn compile_source(source: &str) -> Result<Program, VmError> {
    let tokens = Lexer::new(source).lex()?;
    let declarations = Parser::new(tokens).parse_program()?;
    let mut signatures = HashMap::new();
    for (index, declaration) in declarations.iter().enumerate() {
        if signatures.insert(declaration.name.clone(), Signature {
            id: FunctionId(index),
            params: declaration.params.iter().map(|(_, ty)| *ty).collect(),
            result: declaration.result,
        }).is_some() {
            return Err(VmError::new(format!("duplicate function '{}'", declaration.name)));
        }
    }
    let entry = signatures.get("main").ok_or_else(|| VmError::new("source program must define fn main"))?;
    if !entry.params.is_empty() { return Err(VmError::new("reference runner requires main() without parameters")); }
    if !matches!(entry.result, Type::I64 | Type::Unit) { return Err(VmError::new("main must return an integer or void")); }

    let mut functions = Vec::new();
    for declaration in &declarations {
        let mut compiler = FunctionCompiler::new(declaration, &signatures)?;
        compiler.statement(&declaration.body)?;
        if declaration.result == Type::Unit {
            compiler.code.push(Instruction::Return { value: None });
        }
        let function = Function {
            name: declaration.name.clone(),
            params: declaration.params.iter().map(|(_, ty)| *ty).collect(),
            result: declaration.result,
            registers: compiler.register_types,
            code: compiler.code,
        };
        validate_control_flow(&function)?;
        functions.push(function);
    }
    let program = Program { functions, entry: entry.id };
    program.verify()?;
    Ok(program)
}

struct FunctionCompiler<'a> {
    declaration: &'a FunctionAst,
    signatures: &'a HashMap<String, Signature>,
    code: Vec<Instruction>,
    register_types: Vec<Type>,
    scopes: Vec<HashMap<String, Binding>>,
}
impl<'a> FunctionCompiler<'a> {
    fn new(declaration: &'a FunctionAst, signatures: &'a HashMap<String, Signature>) -> Result<Self, VmError> {
        let mut scope = HashMap::new();
        let register_types = declaration.params.iter().enumerate().map(|(_, (_, ty))| *ty).collect::<Vec<_>>();
        for (index, (name, ty)) in declaration.params.iter().enumerate() {
            if scope.insert(name.clone(), Binding { register: Register(index), ty: *ty, mutable: false }).is_some() {
                return Err(VmError::new(format!("function '{}': duplicate parameter '{name}'", declaration.name)));
            }
        }
        Ok(Self { declaration, signatures, code: Vec::new(), register_types, scopes: vec![scope] })
    }
    fn alloc(&mut self, ty: Type) -> Register {
        let register = Register(self.register_types.len());
        self.register_types.push(ty);
        register
    }
    fn emit(&mut self, instruction: Instruction) -> usize {
        let index = self.code.len(); self.code.push(instruction); index
    }
    fn patch_jump(&mut self, index: usize, target: usize) -> Result<(), VmError> {
        match self.code.get_mut(index) {
            Some(Instruction::Jump { target: slot }) | Some(Instruction::JumpIfFalse { target: slot, .. }) => { *slot = target; Ok(()) }
            _ => Err(VmError::new("internal compiler error: attempted to patch a non-jump")),
        }
    }
    fn lookup(&self, name: &str) -> Option<Binding> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name).cloned())
    }
    fn statement(&mut self, statement: &Stmt) -> Result<(), VmError> {
        match statement {
            Stmt::Block(statements) => {
                self.scopes.push(HashMap::new());
                for statement in statements { self.statement(statement)?; }
                self.scopes.pop();
            }
            Stmt::Let { name, declared, mutable, value } => {
                if self.scopes.last().is_some_and(|scope| scope.contains_key(name)) {
                    return Err(VmError::new(format!("function '{}': duplicate local '{name}' in this scope", self.declaration.name)));
                }
                let (source, actual_type) = self.expression(value)?;
                let ty = declared.unwrap_or(actual_type);
                if ty == Type::Unit { return Err(VmError::new("cannot bind a void expression to a local")); }
                if ty != actual_type { return Err(VmError::new(format!("local '{name}' expects {ty:?}, got {actual_type:?}"))); }
                let register = self.alloc(ty);
                self.emit(Instruction::Copy { dst: register, src: source });
                self.scopes.last_mut().expect("function always has a scope").insert(name.clone(), Binding { register, ty, mutable: *mutable });
            }
            Stmt::Assign { name, value } => {
                let binding = self.lookup(name).ok_or_else(|| VmError::new(format!("unknown variable '{name}'")))?;
                if !binding.mutable { return Err(VmError::new(format!("cannot assign to immutable variable '{name}'"))); }
                let (source, ty) = self.expression(value)?;
                if binding.ty != ty { return Err(VmError::new(format!("assignment to '{name}' expects {:?}, got {ty:?}", binding.ty))); }
                self.emit(Instruction::Copy { dst: binding.register, src: source });
            }
            Stmt::Return(value) => match value {
                None if self.declaration.result == Type::Unit => { self.emit(Instruction::Return { value: None }); }
                None => return Err(VmError::new(format!("function '{}' must return {:?}", self.declaration.name, self.declaration.result))),
                Some(expression) => {
                    let (register, ty) = self.expression(expression)?;
                    if ty != self.declaration.result { return Err(VmError::new(format!("function '{}' returns {:?}, but expression has type {ty:?}", self.declaration.name, self.declaration.result))); }
                    self.emit(if ty == Type::Unit { Instruction::Return { value: None } } else { Instruction::Return { value: Some(register) } });
                }
            },
            Stmt::Expression(expression) => { self.expression(expression)?; }
            Stmt::If { condition, then_branch, else_branch } => {
                let (condition, ty) = self.expression(condition)?;
                if ty != Type::Bool { return Err(VmError::new("if condition must be bool")); }
                let branch = self.emit(Instruction::JumpIfFalse { condition, target: usize::MAX });
                self.statement(then_branch)?;
                if let Some(else_branch) = else_branch {
                    let end_jump = if always_returns(then_branch) { None } else { Some(self.emit(Instruction::Jump { target: usize::MAX })) };
                    let else_start = self.code.len(); self.patch_jump(branch, else_start)?;
                    self.statement(else_branch)?;
                    if let Some(jump) = end_jump { let end = self.code.len(); self.patch_jump(jump, end)?; }
                } else {
                    let end = self.code.len(); self.patch_jump(branch, end)?;
                }
            }
            Stmt::While { condition, body } => {
                let start = self.code.len();
                let (condition, ty) = self.expression(condition)?;
                if ty != Type::Bool { return Err(VmError::new("while condition must be bool")); }
                let exit = self.emit(Instruction::JumpIfFalse { condition, target: usize::MAX });
                self.statement(body)?;
                self.emit(Instruction::Jump { target: start });
                let end = self.code.len(); self.patch_jump(exit, end)?;
            }
        }
        Ok(())
    }
    fn expression(&mut self, expression: &Expr) -> Result<(Register, Type), VmError> {
        match expression {
            Expr::String(value) => {
                let dst = self.alloc(Type::Str);
                self.emit(Instruction::ConstStr { dst, value: value.clone() });
                Ok((dst, Type::Str))
            }
            Expr::Int(value) => {
                let dst = self.alloc(Type::I64); self.emit(Instruction::ConstI64 { dst, value: *value }); Ok((dst, Type::I64))
            }
            Expr::Bool(value) => {
                let dst = self.alloc(Type::Bool); self.emit(Instruction::ConstBool { dst, value: *value }); Ok((dst, Type::Bool))
            }
            Expr::Variable(name) => {
                let binding = self.lookup(name).ok_or_else(|| VmError::new(format!("unknown variable '{name}'")))?;
                Ok((binding.register, binding.ty))
            }
            Expr::Unary { op, value } => {
                let (source, ty) = self.expression(value)?;
                match (op.as_str(), ty) {
                    ("-", Type::I64) => {
                        let zero = self.alloc(Type::I64); self.emit(Instruction::ConstI64 { dst: zero, value: 0 });
                        let dst = self.alloc(Type::I64); self.emit(Instruction::SubI64 { dst, lhs: zero, rhs: source }); Ok((dst, Type::I64))
                    }
                    ("!", Type::Bool) => {
                        let dst = self.alloc(Type::Bool); self.emit(Instruction::NotBool { dst, src: source }); Ok((dst, Type::Bool))
                    }
                    _ => Err(VmError::new(format!("unary '{op}' is not defined for {ty:?}"))),
                }
            }
            Expr::Binary { left, op, right } => {
                let (lhs, lhs_ty) = self.expression(left)?;
                if op == "&&" || op == "||" { return self.logical(op, lhs, lhs_ty, right); }
                let (rhs, rhs_ty) = self.expression(right)?;
                if lhs_ty != rhs_ty { return Err(VmError::new(format!("operator '{op}' has mismatched types {lhs_ty:?} and {rhs_ty:?}"))); }
                let dst_type = match op.as_str() {
                    "+" | "-" | "*" | "/" if lhs_ty == Type::I64 => Type::I64,
                    "==" | "!=" | "<" | "<=" | ">" | ">=" if lhs_ty == Type::I64 => Type::Bool,
                    _ => return Err(VmError::new(format!("operator '{op}' is not supported for {lhs_ty:?}"))),
                };
                let dst = self.alloc(dst_type);
                match op.as_str() {
                    "+" => { self.emit(Instruction::AddI64 { dst, lhs, rhs }); }
                    "-" => { self.emit(Instruction::SubI64 { dst, lhs, rhs }); }
                    "*" => { self.emit(Instruction::MulI64 { dst, lhs, rhs }); }
                    "/" => { self.emit(Instruction::DivI64 { dst, lhs, rhs }); }
                    "==" | "!=" => {
                        self.emit(Instruction::EqI64 { dst, lhs, rhs });
                        if op == "!=" { let negated = self.alloc(Type::Bool); self.emit(Instruction::NotBool { dst: negated, src: dst }); return Ok((negated, Type::Bool)); }
                    }
                    "<" => { self.emit(Instruction::LtI64 { dst, lhs, rhs }); }
                    ">" => { self.emit(Instruction::LtI64 { dst, lhs: rhs, rhs: lhs }); }
                    "<=" => {
                        self.emit(Instruction::LtI64 { dst, lhs: rhs, rhs: lhs });
                        let negated = self.alloc(Type::Bool); self.emit(Instruction::NotBool { dst: negated, src: dst }); return Ok((negated, Type::Bool));
                    }
                    ">=" => {
                        self.emit(Instruction::LtI64 { dst, lhs, rhs });
                        let negated = self.alloc(Type::Bool); self.emit(Instruction::NotBool { dst: negated, src: dst }); return Ok((negated, Type::Bool));
                    }
                    _ => unreachable!("operator was checked above"),
                }
                Ok((dst, dst_type))
            }
            Expr::Call { name, args } => {
                if name == "print" {
                    if args.len() != 1 { return Err(VmError::new("print expects exactly one argument")); }
                    let (source, ty) = self.expression(&args[0])?;
                    match ty {
                        Type::I64 => { self.emit(Instruction::PrintI64 { src: source }); }
                        Type::Bool => { self.emit(Instruction::PrintBool { src: source }); }
                        Type::Str => { self.emit(Instruction::PrintStr { src: source }); }
                        Type::Unit => return Err(VmError::new("print cannot print void")),
                    }
                    let dummy = self.alloc(Type::Unit);
                    return Ok((dummy, Type::Unit));
                }
                let signature = self.signatures.get(name).cloned().ok_or_else(|| VmError::new(format!("unknown function '{name}'")))?;
                if args.len() != signature.params.len() {
                    return Err(VmError::new(format!("function '{name}' expects {} arguments, got {}", signature.params.len(), args.len())));
                }
                let mut argument_registers = Vec::new();
                for (argument, expected) in args.iter().zip(&signature.params) {
                    let (register, actual) = self.expression(argument)?;
                    if actual != *expected { return Err(VmError::new(format!("argument to '{name}' expects {expected:?}, got {actual:?}"))); }
                    argument_registers.push(register);
                }
                if signature.result == Type::Unit {
                    self.emit(Instruction::Call { dst: None, function: signature.id, args: argument_registers });
                    let dummy = self.alloc(Type::Unit);
                    Ok((dummy, Type::Unit))
                } else {
                    let result = self.alloc(signature.result);
                    self.emit(Instruction::Call { dst: Some(result), function: signature.id, args: argument_registers });
                    Ok((result, signature.result))
                }
            }
        }
    }
    fn logical(&mut self, op: &str, lhs: Register, lhs_type: Type, right: &Expr) -> Result<(Register, Type), VmError> {
        if lhs_type != Type::Bool { return Err(VmError::new(format!("operator '{op}' requires bool operands"))); }
        let dst = self.alloc(Type::Bool);
        let branch = self.emit(Instruction::JumpIfFalse { condition: lhs, target: usize::MAX });
        if op == "&&" {
            let (rhs, rhs_type) = self.expression(right)?;
            if rhs_type != Type::Bool { return Err(VmError::new("'&&' requires bool operands")); }
            self.emit(Instruction::Copy { dst, src: rhs });
            let end_jump = self.emit(Instruction::Jump { target: usize::MAX });
            let false_target = self.code.len(); self.patch_jump(branch, false_target)?;
            self.emit(Instruction::ConstBool { dst, value: false });
            let end = self.code.len(); self.patch_jump(end_jump, end)?;
        } else {
            self.emit(Instruction::ConstBool { dst, value: true });
            let end_jump = self.emit(Instruction::Jump { target: usize::MAX });
            let rhs_start = self.code.len(); self.patch_jump(branch, rhs_start)?;
            let (rhs, rhs_type) = self.expression(right)?;
            if rhs_type != Type::Bool { return Err(VmError::new("'||' requires bool operands")); }
            self.emit(Instruction::Copy { dst, src: rhs });
            let end = self.code.len(); self.patch_jump(end_jump, end)?;
        }
        Ok((dst, Type::Bool))
    }
}

fn always_returns(statement: &Stmt) -> bool {
    match statement {
        Stmt::Return(_) => true,
        Stmt::Block(statements) => statements.last().is_some_and(always_returns),
        Stmt::If { then_branch, else_branch: Some(else_branch), .. } => always_returns(then_branch) && always_returns(else_branch),
        _ => false,
    }
}

fn validate_control_flow(function: &Function) -> Result<(), VmError> {
    let mut visited = vec![false; function.code.len()];
    let mut pending = vec![0usize];
    while let Some(pc) = pending.pop() {
        if pc >= function.code.len() {
            return Err(VmError::new(format!("function '{}' can fall off the end without returning", function.name)));
        }
        if visited[pc] { continue; }
        visited[pc] = true;
        match &function.code[pc] {
            Instruction::Return { .. } => {}
            Instruction::Jump { target } => pending.push(*target),
            Instruction::JumpIfFalse { target, .. } => { pending.push(*target); pending.push(pc + 1); }
            _ => pending.push(pc + 1),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Value;

    #[test]
    fn compiles_ramlang_style_functions_and_prints_results() {
        let source = r#"
            fn square(x: i32) -> i32 { return x * x; }
            fn main() -> i32 {
                let answer: i32 = square(7);
                print(answer);
                return 0;
            }
        "#;
        let program = compile_source(source).unwrap();
        let (result, output) = program.run_capture(&[], 1000).unwrap();
        assert_eq!(result, Value::I64(0));
        assert_eq!(output, vec!["49"]);
    }

    #[test]
    fn lowers_mutable_locals_loops_and_short_circuit_logic() {
        let source = r#"
            fn main() -> i32 {
                let mut i: i32 = 0;
                let mut sum: i32 = 0;
                while (i < 5) {
                    sum = sum + i;
                    i = i + 1;
                }
                if (sum == 10 && i >= 5) { print(sum); } else { print(0); }
                return 0;
            }
        "#;
        let (result, output) = compile_source(source).unwrap().run_capture(&[], 1000).unwrap();
        assert_eq!(result, Value::I64(0));
        assert_eq!(output, vec!["10"]);
    }


    #[test]
    fn compiles_and_prints_utf8_string_literals_and_string_return_values() {
        let source = r#"
            fn greeting() -> str {
                return "Ramlang says: \"hello\", café";
            }
            fn main() -> i32 {
                print(greeting());
                print("line\nseparator");
                return 0;
            }
        "#;
        let (result, output) = compile_source(source).unwrap().run_capture(&[], 1000).unwrap();
        assert_eq!(result, Value::I64(0));
        assert_eq!(output, vec!["Ramlang says: \"hello\", café", "line\nseparator"]);
    }

    #[test]
    fn rejects_unterminated_and_unknown_string_escapes() {
        assert!(compile_source("fn main() { print(\"open); }").unwrap_err().to_string().contains("unterminated string"));
        assert!(compile_source(r#"fn main() { print("bad\q"); }"#).unwrap_err().to_string().contains("unsupported string escape"));
    }

    #[test]
    fn rejects_unsafe_subset_gaps_with_diagnostics() {
        let error = compile_source("fn main() -> i32 { missing(); return 0; }").unwrap_err();
        assert!(error.to_string().contains("unknown function"));
    }
}
