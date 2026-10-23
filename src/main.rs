use std::{
    collections::HashMap,
    env, fs,
    path::PathBuf,
    process::{self, Command},
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenKind {
    Ident,
    Number,
    String,
    Symbol(String),
    Eof,
}

#[derive(Debug, Clone)]
struct Token {
    kind: TokenKind,
    text: String,
    line: usize,
    column: usize,
}

struct Lexer<'a> {
    source: &'a [u8],
    index: usize,
    line: usize,
    column: usize,
}

impl<'a> Lexer<'a> {
    fn new(source: &'a str) -> Self {
        Self { source: source.as_bytes(), index: 0, line: 1, column: 1 }
    }

    fn peek(&self, offset: usize) -> Option<u8> {
        self.source.get(self.index + offset).copied()
    }

    fn advance(&mut self) -> Option<u8> {
        let byte = self.peek(0)?;
        self.index += 1;
        if byte == b'\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(byte)
    }

    fn token(&self, kind: TokenKind, text: String, line: usize, column: usize) -> Token {
        Token { kind, text, line, column }
    }

    fn lex(mut self) -> Result<Vec<Token>, String> {
        let mut tokens = Vec::new();
        while let Some(byte) = self.peek(0) {
            if byte.is_ascii_whitespace() {
                self.advance();
                continue;
            }

            let line = self.line;
            let column = self.column;
            let start = self.index;

            if byte.is_ascii_alphabetic() || byte == b'_' {
                self.advance();
                while self.peek(0).is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_') {
                    self.advance();
                }
                let text = String::from_utf8(self.source[start..self.index].to_vec()).unwrap();
                tokens.push(self.token(TokenKind::Ident, text, line, column));
                continue;
            }

            if byte.is_ascii_digit() {
                self.advance();
                while self.peek(0).is_some_and(|b| b.is_ascii_digit()) {
                    self.advance();
                }
                if self.peek(0) == Some(b'.') && self.peek(1).is_some_and(|b| b.is_ascii_digit()) {
                    self.advance();
                    while self.peek(0).is_some_and(|b| b.is_ascii_digit()) {
                        self.advance();
                    }
                }
                let text = String::from_utf8(self.source[start..self.index].to_vec()).unwrap();
                tokens.push(self.token(TokenKind::Number, text, line, column));
                continue;
            }

            if byte == b'"' {
                self.advance();
                let mut bytes = Vec::new();
                let mut closed = false;
                while let Some(next) = self.peek(0) {
                    match next {
                        b'"' => {
                            self.advance();
                            closed = true;
                            break;
                        }
                        b'\n' => return Err(format!("unescaped newline in string at {line}:{column}")),
                        b'\\' => {
                            self.advance();
                            let escaped = self.peek(0).ok_or_else(|| format!("unterminated escape sequence at {line}:{column}"))?;
                            self.advance();
                            match escaped {
                                b'n' => bytes.push(b'\n'),
                                b'r' => bytes.push(b'\r'),
                                b't' => bytes.push(b'\t'),
                                b'0' => bytes.push(0),
                                b'\\' => bytes.push(b'\\'),
                                b'"' => bytes.push(b'"'),
                                _ => return Err(format!("unsupported string escape at {line}:{column}")),
                            }
                        }
                        _ => bytes.push(self.advance().unwrap()),
                    }
                }
                if !closed {
                    return Err(format!("unterminated string at {line}:{column}"));
                }
                let value = String::from_utf8(bytes)
                    .map_err(|_| format!("string literal is not valid UTF-8 at {line}:{column}"))?;
                tokens.push(self.token(TokenKind::String, value, line, column));
                continue;
            }

            if byte == b'/' && self.peek(1) == Some(b'/') {
                self.advance();
                self.advance();
                while self.peek(0).is_some_and(|b| b != b'\n') {
                    self.advance();
                }
                continue;
            }

            if byte == b'/' && self.peek(1) == Some(b'*') {
                self.advance();
                self.advance();
                let mut closed = false;
                while self.peek(0).is_some() {
                    if self.peek(0) == Some(b'*') && self.peek(1) == Some(b'/') {
                        self.advance();
                        self.advance();
                        closed = true;
                        break;
                    }
                    self.advance();
                }
                if !closed {
                    return Err(format!("unterminated block comment at {line}:{column}"));
                }
                continue;
            }

            let pair = match (byte, self.peek(1)) {
                (b'&', Some(b'&')) => Some("&&"),
                (b'|', Some(b'|')) => Some("||"),
                (b'<', Some(b'<')) => Some("<<"),
                (b'>', Some(b'>')) => Some(">>"),
                (b'-', Some(b'>')) => Some("->"),
                (b'=', Some(b'=')) => Some("=="),
                (b'!', Some(b'=')) => Some("!="),
                (b'<', Some(b'=')) => Some("<="),
                (b'>', Some(b'=')) => Some(">="),
                _ => None,
            };
            if let Some(symbol) = pair {
                self.advance();
                self.advance();
                tokens.push(self.token(TokenKind::Symbol(symbol.to_owned()), symbol.to_owned(), line, column));
                continue;
            }

            let symbol = match byte {
                b'(' => "(", b')' => ")", b'{' => "{", b'}' => "}",
                b'[' => "[", b']' => "]", b'.' => ".",
                b':' => ":", b';' => ";", b',' => ",", b'+' => "+",
                b'-' => "-", b'*' => "*", b'/' => "/", b'%' => "%", b'=' => "=",
                b'<' => "<", b'>' => ">", b'!' => "!",
                b'&' => "&", b'|' => "|", b'^' => "^", b'~' => "~",
                _ => {
                    let ch = if byte.is_ascii() { char::from(byte).to_string() } else { format!("byte 0x{byte:02x}") };
                    return Err(format!("invalid character {ch} at {line}:{column}; RamLang currently accepts ASCII source"));
                }
            };
            self.advance();
            tokens.push(self.token(TokenKind::Symbol(symbol.to_owned()), symbol.to_owned(), line, column));
        }

        tokens.push(Token { kind: TokenKind::Eof, text: String::new(), line: self.line, column: self.column });
        Ok(tokens)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArrayElement {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F64,
    Bool,
    Ptr,
    Str,
}

impl ArrayElement {
    fn from_type(ty: Type) -> Option<Self> {
        match ty {
            Type::I8 => Some(Self::I8),
            Type::I16 => Some(Self::I16),
            Type::I32 => Some(Self::I32),
            Type::I64 => Some(Self::I64),
            Type::U8 => Some(Self::U8),
            Type::U16 => Some(Self::U16),
            Type::U32 => Some(Self::U32),
            Type::U64 => Some(Self::U64),
            Type::F64 => Some(Self::F64),
            Type::Bool => Some(Self::Bool),
            Type::Ptr => Some(Self::Ptr),
            Type::Str => Some(Self::Str),
            _ => None,
        }
    }

    fn ty(self) -> Type {
        match self {
            Self::I8 => Type::I8,
            Self::I16 => Type::I16,
            Self::I32 => Type::I32,
            Self::I64 => Type::I64,
            Self::U8 => Type::U8,
            Self::U16 => Type::U16,
            Self::U32 => Type::U32,
            Self::U64 => Type::U64,
            Self::F64 => Type::F64,
            Self::Bool => Type::Bool,
            Self::Ptr => Type::Ptr,
            Self::Str => Type::Str,
        }
    }

    fn llvm(self) -> &'static str {
        self.ty().llvm()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Type {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F64,
    Bool,
    Str,
    Ptr,
    Array(ArrayElement, u32),
    Struct(u64),
    Void,
}

impl Type {
    fn llvm(self) -> &'static str {
        match self {
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::U8 => "i8",
            Self::U16 => "i16",
            Self::U32 => "i32",
            Self::U64 => "i64",
            Self::F64 => "double",
            Self::Bool => "i1",
            Self::Str => "ptr",
            Self::Ptr => "ptr",
            Self::Array(_, _) => "ptr",
            Self::Struct(_) => "ptr",
            Self::Void => "void",
        }
    }

    fn is_numeric(self) -> bool {
        matches!(self, Self::I8 | Self::I16 | Self::I32 | Self::I64 | Self::U8 | Self::U16 | Self::U32 | Self::U64 | Self::F64)
    }

    fn is_integer(self) -> bool {
        matches!(self, Self::I8 | Self::I16 | Self::I32 | Self::I64 | Self::U8 | Self::U16 | Self::U32 | Self::U64)
    }

    fn is_unsigned(self) -> bool {
        matches!(self, Self::U8 | Self::U16 | Self::U32 | Self::U64)
    }

    fn integer_width(self) -> Option<u32> {
        match self {
            Self::I8 | Self::U8 => Some(8),
            Self::I16 | Self::U16 => Some(16),
            Self::I32 | Self::U32 => Some(32),
            Self::I64 | Self::U64 => Some(64),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct Function {
    name: String,
    params: Vec<(String, Type)>,
    return_type: Type,
    body: Option<Stmt>,
    external: bool,
}

#[derive(Debug, Clone)]
struct StructDef {
    id: u64,
    name: String,
    fields: Vec<(String, Type)>,
}

#[derive(Debug, Clone, Default)]
struct Program {
    functions: Vec<Function>,
    structs: Vec<StructDef>,
}

fn struct_type_id(name: &str) -> u64 {
    name.bytes().fold(14_695_981_039_346_656_037_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(1_099_511_628_211)
    })
}

#[derive(Debug, Clone)]
enum Stmt {
    Block(Vec<Stmt>),
    Let { name: String, declared_type: Option<Type>, mutable: bool, value: Expr },
    Assign { name: String, value: Expr },
    FieldAssign { base: Expr, field: String, value: Expr },
    IndexAssign { array: Expr, index: Expr, value: Expr },
    Return(Option<Expr>),
    Break,
    Continue,
    Expression(Expr),
    If { condition: Expr, then_branch: Box<Stmt>, else_branch: Option<Box<Stmt>> },
    While { condition: Expr, body: Box<Stmt> },
    For { initializer: Box<Stmt>, condition: Expr, increment: Box<Stmt>, body: Box<Stmt> },
}

#[derive(Debug, Clone)]
enum Expr {
    Number(String),
    ArrayLiteral(Vec<Expr>),
    String(String),
    Bool(bool),
    Null,
    Variable(String),
    StructLiteral { type_id: u64, fields: Vec<(String, Expr)> },
    Field { base: Box<Expr>, field: String },
    Unary { op: String, right: Box<Expr> },
    Cast { value: Box<Expr>, target: Type },
    Index { array: Box<Expr>, index: Box<Expr> },
    Binary { left: Box<Expr>, op: String, right: Box<Expr> },
    Call { name: String, args: Vec<Expr> },
}

struct Parser {
    tokens: Vec<Token>,
    index: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, index: 0 }
    }

    fn current(&self) -> &Token {
        &self.tokens[self.index]
    }

    fn at_end(&self) -> bool {
        matches!(self.current().kind, TokenKind::Eof)
    }

    fn check_symbol(&self, symbol: &str) -> bool {
        self.current().kind == TokenKind::Symbol(symbol.to_owned())
    }

    fn match_symbol(&mut self, symbol: &str) -> bool {
        if self.check_symbol(symbol) {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn match_keyword(&mut self, keyword: &str) -> bool {
        if matches!(self.current().kind, TokenKind::Ident) && self.current().text == keyword {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn expect_symbol(&mut self, symbol: &str) -> Result<(), String> {
        if self.match_symbol(symbol) {
            Ok(())
        } else {
            Err(self.error(format!("expected '{symbol}'")))
        }
    }

    fn expect_ident(&mut self, purpose: &str) -> Result<String, String> {
        if matches!(self.current().kind, TokenKind::Ident) {
            let result = self.current().text.clone();
            self.index += 1;
            Ok(result)
        } else {
            Err(self.error(format!("expected {purpose}")))
        }
    }

    fn error(&self, message: String) -> String {
        format!("{message} at {}:{}", self.current().line, self.current().column)
    }

    fn parse_program(&mut self) -> Result<Program, String> {
        let mut program = Program::default();
        while !self.at_end() {
            if self.match_keyword("struct") {
                program.structs.push(self.parse_struct()?);
                continue;
            }
            let external = self.match_keyword("extern");
            if !self.match_keyword("fn") {
                return Err(self.error("expected a struct declaration, function declaration, or 'extern fn'".to_owned()));
            }
            program.functions.push(self.parse_function(external)?);
        }
        if program.functions.is_empty() && program.structs.is_empty() {
            return Err("source file does not contain any declarations".to_owned());
        }
        Ok(program)
    }

    fn parse_struct(&mut self) -> Result<StructDef, String> {
        let name = self.expect_ident("struct name")?;
        self.expect_symbol("{")?;
        let mut fields = Vec::new();
        while !self.check_symbol("}") && !self.at_end() {
            let field_name = self.expect_ident("field name")?;
            self.expect_symbol(":")?;
            let field_type = self.parse_type()?;
            if field_type == Type::Void {
                return Err(self.error("a struct field cannot have type void".to_owned()));
            }
            if fields.iter().any(|(existing, _)| existing == &field_name) {
                return Err(self.error(format!("duplicate field '{field_name}' in struct '{name}'")));
            }
            fields.push((field_name, field_type));
            if !self.match_symbol(",") {
                break;
            }
        }
        self.expect_symbol("}")?;
        if fields.is_empty() {
            return Err(self.error(format!("struct '{name}' must contain at least one field")));
        }
        Ok(StructDef { id: struct_type_id(&name), name, fields })
    }

    fn parse_type(&mut self) -> Result<Type, String> {
        if self.match_symbol("[") {
            let element_type = self.parse_type()?;
            self.expect_symbol(";")?;
            if !matches!(self.current().kind, TokenKind::Number) || self.current().text.contains('.') {
                return Err(self.error("array length must be a positive integer literal".to_owned()));
            }
            let length: u32 = self.current().text.parse()
                .map_err(|_| self.error("array length exceeds the supported u32 range".to_owned()))?;
            if length == 0 {
                return Err(self.error("array length must be greater than zero".to_owned()));
            }
            self.index += 1;
            self.expect_symbol("]")?;
            let element = ArrayElement::from_type(element_type)
                .ok_or_else(|| self.error("array element type must be a non-void scalar or pointer type".to_owned()))?;
            return Ok(Type::Array(element, length));
        }
        let name = self.expect_ident("type name")?;
        match name.as_str() {
            "i8" => Ok(Type::I8),
            "i16" => Ok(Type::I16),
            "i32" => Ok(Type::I32),
            "i64" => Ok(Type::I64),
            "u8" => Ok(Type::U8),
            "u16" => Ok(Type::U16),
            "u32" => Ok(Type::U32),
            "u64" => Ok(Type::U64),
            "f64" => Ok(Type::F64),
            "bool" => Ok(Type::Bool),
            "str" => Ok(Type::Str),
            "ptr" => Ok(Type::Ptr),
            "void" => Ok(Type::Void),
            _ => Ok(Type::Struct(struct_type_id(&name)),
        }
    }

    fn parse_function(&mut self, external: bool) -> Result<Function, String> {
        let name = self.expect_ident("function name")?;
        self.expect_symbol("(")?;
        let mut params = Vec::new();
        if !self.check_symbol(")") {
            loop {
                let param_name = self.expect_ident("parameter name")?;
                self.expect_symbol(":")?;
                let param_type = self.parse_type()?;
                if param_type == Type::Void {
                    return Err(self.error("a parameter cannot have type void".to_owned()));
                }
                params.push((param_name, param_type));
                if !self.match_symbol(",") {
                    break;
                }
            }
        }
        self.expect_symbol(")")?;
        let return_type = if self.match_symbol("->") { self.parse_type()? } else { Type::Void };
        let body = if external {
            self.expect_symbol(";")?;
            None
        } else {
            Some(self.parse_block()?)
        };
        Ok(Function { name, params, return_type, body, external })
    }

    fn parse_block(&mut self) -> Result<Stmt, String> {
        self.expect_symbol("{")?;
        let mut statements = Vec::new();
        while !self.check_symbol("}") && !self.at_end() {
            statements.push(self.parse_statement()?);
        }
        if self.at_end() {
            return Err(self.error("unterminated block; expected '}'".to_owned()));
        }
        self.expect_symbol("}")?;
        Ok(Stmt::Block(statements))
    }

    fn parse_statement(&mut self) -> Result<Stmt, String> {
        if self.check_symbol("{") {
            return self.parse_block();
        }
        if self.match_keyword("let") {
            let mutable = self.match_keyword("mut");
            let name = self.expect_ident("variable name")?;
            let declared_type = if self.match_symbol(":") { Some(self.parse_type()?) } else { None };
            if declared_type == Some(Type::Void) {
                return Err(self.error("a variable cannot have type void".to_owned()));
            }
            self.expect_symbol("=")?;
            let value = self.parse_expression()?;
            self.expect_symbol(";")?;
            return Ok(Stmt::Let { name, declared_type, mutable, value });
        }
        if self.match_keyword("return") {
            let value = if self.check_symbol(";") { None } else { Some(self.parse_expression()?) };
            self.expect_symbol(";")?;
            return Ok(Stmt::Return(value));
        }
        if self.match_keyword("break") {
            self.expect_symbol(";")?;
            return Ok(Stmt::Break);
        }
        if self.match_keyword("continue") {
            self.expect_symbol(";")?;
            return Ok(Stmt::Continue);
        }
        if self.match_keyword("if") {
            self.expect_symbol("(")?;
            let condition = self.parse_expression()?;
            self.expect_symbol(")")?;
            let then_branch = Box::new(self.parse_block()?);
            let else_branch = if self.match_keyword("else") {
                Some(Box::new(if self.match_keyword("if") {
                    self.index -= 1;
                    self.parse_statement()?
                } else {
                    self.parse_block()?
                }))
            } else {
                None
            };
            return Ok(Stmt::If { condition, then_branch, else_branch });
        }
        if self.match_keyword("while") {
            self.expect_symbol("(")?;
            let condition = self.parse_expression()?;
            self.expect_symbol(")")?;
            let body = Box::new(self.parse_block()?);
            return Ok(Stmt::While { condition, body });
        }
        if self.match_keyword("for") {
            self.expect_symbol("(")?;
            let initializer = self.parse_statement()?;
            if !matches!(initializer, Stmt::Let { .. } | Stmt::Assign { .. }) {
                return Err(self.error("for-loop initializer must be a variable declaration or assignment".to_owned()));
            }
            let condition = self.parse_expression()?;
            self.expect_symbol(";")?;
            let increment = self.parse_for_increment()?;
            self.expect_symbol(")")?;
            let body = Box::new(self.parse_block()?);
            return Ok(Stmt::For {
                initializer: Box::new(initializer),
                condition,
                increment: Box::new(increment),
                body,
            });
        }
        if matches!(self.current().kind, TokenKind::Ident)
            && self.tokens.get(self.index + 1).is_some_and(|t| t.kind == TokenKind::Symbol("[".to_owned()))
        {
            let saved_index = self.index;
            let name = self.current().text.clone();
            self.index += 1;
            self.expect_symbol("[")?;
            let index = self.parse_expression()?;
            self.expect_symbol("]")?;
            if self.match_symbol("=") {
                let value = self.parse_expression()?;
                self.expect_symbol(";")?;
                return Ok(Stmt::IndexAssign { array: Expr::Variable(name), index, value });
            }
            self.index = saved_index;
        }
        if matches!(self.current().kind, TokenKind::Ident)
            && self.tokens.get(self.index + 1).is_some_and(|t| t.kind == TokenKind::Symbol("=".to_owned()))
        {
            let name = self.current().text.clone();
            self.index += 2;
            let value = self.parse_expression()?;
            self.expect_symbol(";")?;
            return Ok(Stmt::Assign { name, value });
        }
        let expression = self.parse_expression()?;
        self.expect_symbol(";")?;
        Ok(Stmt::Expression(expression))
    }

    fn parse_for_increment(&mut self) -> Result<Stmt, String> {
        if matches!(self.current().kind, TokenKind::Ident)
            && self.tokens.get(self.index + 1).is_some_and(|token| token.kind == TokenKind::Symbol("=".to_owned()))
        {
            let name = self.current().text.clone();
            self.index += 2;
            let value = self.parse_expression()?;
            return Ok(Stmt::Assign { name, value });
        }
        Ok(Stmt::Expression(self.parse_expression()?))
    }

    fn parse_expression(&mut self) -> Result<Expr, String> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_and()?;
        while self.match_symbol("||") {
            let right = self.parse_and()?;
            expr = Expr::Binary { left: Box::new(expr), op: "||".to_owned(), right: Box::new(right) };
        }
        Ok(expr)
    }

    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_bit_or()?;
        while self.match_symbol("&&") {
            let right = self.parse_bit_or()?;
            expr = Expr::Binary { left: Box::new(expr), op: "&&".to_owned(), right: Box::new(right) };
        }
        Ok(expr)
    }

    fn parse_bit_or(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_bit_xor()?;
        while self.match_symbol("|") {
            let right = self.parse_bit_xor()?;
            expr = Expr::Binary { left: Box::new(expr), op: "|".to_owned(), right: Box::new(right) };
        }
        Ok(expr)
    }

    fn parse_bit_xor(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_bit_and()?;
        while self.match_symbol("^") {
            let right = self.parse_bit_and()?;
            expr = Expr::Binary { left: Box::new(expr), op: "^".to_owned(), right: Box::new(right) };
        }
        Ok(expr)
    }

    fn parse_bit_and(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_equality()?;
        while self.match_symbol("&") {
            let right = self.parse_equality()?;
            expr = Expr::Binary { left: Box::new(expr), op: "&".to_owned(), right: Box::new(right) };
        }
        Ok(expr)
    }

    fn parse_equality(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_comparison()?;
        while self.check_symbol("==") || self.check_symbol("!=") {
            let op = self.current().text.clone();
            self.index += 1;
            let right = self.parse_comparison()?;
            expr = Expr::Binary { left: Box::new(expr), op, right: Box::new(right) };
        }
        Ok(expr)
    }

    fn parse_comparison(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_shift()?;
        while ["<", "<=", ">", ">="].iter().any(|op| self.check_symbol(op)) {
            let op = self.current().text.clone();
            self.index += 1;
            let right = self.parse_shift()?;
            expr = Expr::Binary { left: Box::new(expr), op, right: Box::new(right) };
        }
        Ok(expr)
    }

    fn parse_shift(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_term()?;
        while self.check_symbol("<<") || self.check_symbol(">>") {
            let op = self.current().text.clone();
            self.index += 1;
            let right = self.parse_term()?;
            expr = Expr::Binary { left: Box::new(expr), op, right: Box::new(right) };
        }
        Ok(expr)
    }

    fn parse_term(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_factor()?;
        while self.check_symbol("+") || self.check_symbol("-") {
            let op = self.current().text.clone();
            self.index += 1;
            let right = self.parse_factor()?;
            expr = Expr::Binary { left: Box::new(expr), op, right: Box::new(right) };
        }
        Ok(expr)
    }

    fn parse_factor(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_unary()?;
        while self.check_symbol("*") || self.check_symbol("/") || self.check_symbol("%") {
            let op = self.current().text.clone();
            self.index += 1;
            let right = self.parse_unary()?;
            expr = Expr::Binary { left: Box::new(expr), op, right: Box::new(right) };
        }
        Ok(expr)
    }

    fn parse_unary(&mut self) -> Result<Expr, String> {
        if self.check_symbol("-") || self.check_symbol("!") || self.check_symbol("~") {
            let op = self.current().text.clone();
            self.index += 1;
            return Ok(Expr::Unary { op, right: Box::new(self.parse_unary()?) });
        }
        self.parse_cast()
    }

    fn parse_cast(&mut self) -> Result<Expr, String> {
        let mut value = self.parse_primary()?;
        loop {
            if self.match_symbol("[") {
                let index = self.parse_expression()?;
                self.expect_symbol("]")?;
                value = Expr::Index { array: Box::new(value), index: Box::new(index) };
            } else if self.match_keyword("as") {
                let target = self.parse_type()?;
                value = Expr::Cast { value: Box::new(value), target };
            } else {
                break;
            }
        }
        Ok(value)
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        if self.match_symbol("[") {
            let mut elements = Vec::new();
            if self.check_symbol("]") {
                return Err(self.error("array literals cannot be empty".to_owned()));
            }
            loop {
                elements.push(self.parse_expression()?);
                if !self.match_symbol(",") {
                    break;
                }
                if self.check_symbol("]") {
                    break;
                }
            }
            self.expect_symbol("]")?;
            return Ok(Expr::ArrayLiteral(elements));
        }
        if self.match_symbol("(") {
            let expr = self.parse_expression()?;
            self.expect_symbol(")")?;
            return Ok(expr);
        }
        if matches!(self.current().kind, TokenKind::Number) {
            let text = self.current().text.clone();
            self.index += 1;
            return Ok(Expr::Number(text));
        }
        if matches!(self.current().kind, TokenKind::String) {
            let text = self.current().text.clone();
            self.index += 1;
            return Ok(Expr::String(text));
        }
        if self.match_keyword("true") {
            return Ok(Expr::Bool(true));
        }
        if self.match_keyword("false") {
            return Ok(Expr::Bool(false));
        }
        if self.match_keyword("null") {
            return Ok(Expr::Null);
        }
        if matches!(self.current().kind, TokenKind::Ident) {
            let name = self.current().text.clone();
            self.index += 1;
            if self.match_symbol("(") {
                let mut args = Vec::new();
                if !self.check_symbol(")") {
                    loop {
                        args.push(self.parse_expression()?);
                        if !self.match_symbol(",") {
                            break;
                        }
                    }
                }
                self.expect_symbol(")")?;
                return Ok(Expr::Call { name, args });
            }
            return Ok(Expr::Variable(name));
        }
        Err(self.error("expected an expression".to_owned()))
    }
}

#[derive(Debug, Clone)]
struct Signature {
    params: Vec<Type>,
    return_type: Type,
}

#[derive(Debug, Clone)]
struct Binding {
    ty: Type,
    slot: String,
    mutable: bool,
}

struct FunctionGenerator<'a> {
    signatures: &'a HashMap<String, Signature>,
    return_type: Type,
    allocas: Vec<String>,
    lines: Vec<String>,
    scopes: Vec<HashMap<String, Binding>>,
    current_block: String,
    function_name: String,
    string_counter: usize,
    string_globals: Vec<String>,
    loop_stack: Vec<(String, String)>,
    next_value: usize,
    next_label: usize,
    terminated: bool,
    uses_printf: bool,
}

impl<'a> FunctionGenerator<'a> {
    fn new(signatures: &'a HashMap<String, Signature>, return_type: Type, function_name: &str) -> Self {
        Self {
            signatures, return_type, allocas: Vec::new(), lines: Vec::new(),
            scopes: vec![HashMap::new()], current_block: "entry".to_owned(),
            function_name: function_name.to_owned(), string_counter: 0,
            string_globals: Vec::new(), loop_stack: Vec::new(),
            next_value: 0, next_label: 0, terminated: false, uses_printf: false,
        }
    }

    fn emit(&mut self, instruction: impl Into<String>) {
        self.lines.push(format!("  {}", instruction.into()));
    }

    fn label(&mut self, name: &str) {
        self.lines.push(format!("{name}:"));
        self.current_block = name.to_owned();
        self.terminated = false;
    }

    fn fresh_value(&mut self) -> String {
        let name = format!("%t{}", self.next_value);
        self.next_value += 1;
        name
    }

    fn fresh_slot(&mut self, ty: Type) -> String {
        let name = format!("%v{}", self.next_value);
        self.next_value += 1;
        self.allocas.push(format!("  {name} = alloca {}", ty.llvm()));
        name
    }

    fn fresh_label(&mut self, prefix: &str) -> String {
        let name = format!("{prefix}.{}", self.next_label);
        self.next_label += 1;
        name
    }

    fn lookup(&self, name: &str) -> Option<Binding> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name).cloned())
    }

    fn require_same(&self, expected: Type, actual: Type, context: &str) -> Result<(), String> {
        if expected == actual {
            Ok(())
        } else {
            Err(format!("{context}: expected {}, found {}", expected.llvm(), actual.llvm()))
        }
    }

    fn emit_array_element_ptr(&mut self, array: &Expr, index: &Expr) -> Result<(String, ArrayElement), String> {
        let (base, array_type) = self.emit_expr(array)?;
        let Type::Array(element, length) = array_type else {
            return Err(format!("indexing requires a fixed-length array, found {}", array_type.llvm()));
        };
        let (index_value, index_type) = self.emit_expr(index)?;
        let Some(width) = index_type.integer_width() else {
            return Err("array index must be an integer".to_owned());
        };
        let index_i64 = if width == 64 {
            index_value
        } else {
            let extended = self.fresh_value();
            let extension = if index_type.is_unsigned() { "zext" } else { "sext" };
            self.emit(format!("{extended} = {extension} {} {index_value} to i64", index_type.llvm()));
            extended
        };

        let in_bounds = self.fresh_value();
        self.emit(format!("{in_bounds} = icmp ult i64 {index_i64}, {length}"));
        let ok_label = self.fresh_label("array.bounds.ok");
        let trap_label = self.fresh_label("array.bounds.trap");
        self.emit(format!("br i1 {in_bounds}, label %{ok_label}, label %{trap_label}"));
        self.terminated = true;

        self.label(&trap_label);
        self.emit("call void @llvm.trap()");
        self.emit("unreachable");
        self.terminated = true;

        self.label(&ok_label);
        let element_ptr = self.fresh_value();
        self.emit(format!(
            "{element_ptr} = getelementptr inbounds {}, ptr {base}, i64 {index_i64}",
            element.llvm()
        ));
        Ok((element_ptr, element))
    }

    fn emit_expr(&mut self, expression: &Expr) -> Result<(String, Type), String> {
        self.emit_expr_expected(expression, None)
    }

    fn emit_logical(&mut self, left: &Expr, op: &str, right: &Expr) -> Result<(String, Type), String> {
        let (left_value, left_type) = self.emit_expr_expected(left, Some(Type::Bool))?;
        self.require_same(Type::Bool, left_type, &format!("left operand of '{op}'"))?;

        let left_block = self.current_block.clone();
        let right_label = self.fresh_label("logic.rhs");
        let end_label = self.fresh_label("logic.end");
        if op == "&&" {
            self.emit(format!("br i1 {left_value}, label %{right_label}, label %{end_label}"));
        } else {
            self.emit(format!("br i1 {left_value}, label %{end_label}, label %{right_label}"));
        }
        self.terminated = true;

        self.label(&right_label);
        let (right_value, right_type) = self.emit_expr_expected(right, Some(Type::Bool))?;
        self.require_same(Type::Bool, right_type, &format!("right operand of '{op}'"))?;
        let right_block = self.current_block.clone();
        self.emit(format!("br label %{end_label}"));
        self.terminated = true;

        self.label(&end_label);
        let result = self.fresh_value();
        let short_value = if op == "&&" { "false" } else { "true" };
        self.emit(format!("{result} = phi i1 [{short_value}, %{left_block}], [{right_value}, %{right_block}]"));
        Ok((result, Type::Bool))
    }

    fn emit_expr_expected(&mut self, expression: &Expr, expected: Option<Type>) -> Result<(String, Type), String> {
        match expression {
            Expr::ArrayLiteral(elements) => {
                let Some(Type::Array(element, length)) = expected else {
                    return Err("array literals require an explicit fixed-length array type, for example [i32; 4]".to_owned());
                };
                if elements.len() != length as usize {
                    return Err(format!("array literal has {} element(s), but its type requires {length}", elements.len()));
                }
                let element_type = element.ty();
                let aggregate_type = format!("[{length} x {}]", element.llvm());
                let storage = self.fresh_value();
                self.allocas.push(format!("  {storage} = alloca {aggregate_type}"));
                for (index, expression) in elements.iter().enumerate() {
                    let (value, actual_type) = self.emit_expr_expected(expression, Some(element_type))?;
                    self.require_same(element_type, actual_type, "array literal element")?;
                    let element_ptr = self.fresh_value();
                    self.emit(format!(
                        "{element_ptr} = getelementptr inbounds {aggregate_type}, ptr {storage}, i64 0, i64 {index}"
                    ));
                    self.emit(format!("store {} {value}, ptr {element_ptr}", element.llvm()));
                }
                let first_element = self.fresh_value();
                self.emit(format!(
                    "{first_element} = getelementptr inbounds {aggregate_type}, ptr {storage}, i64 0, i64 0"
                ));
                Ok((first_element, Type::Array(element, length)))
            }
            Expr::String(value) => {
                let index = self.string_counter;
                self.string_counter += 1;
                let global = format!(".str.{}.{}", self.function_name, index);
                let encoded = value.as_bytes().iter()
                    .map(|byte| format!("\\{:02X}", byte))
                    .collect::<String>();
                let length = value.as_bytes().len() + 1;
                self.string_globals.push(format!(
                    "@{global} = private unnamed_addr constant [{length} x i8] c\"{encoded}\\00\", align 1"
                ));
                let result = self.fresh_value();
                self.emit(format!(
                    "{result} = getelementptr inbounds [{length} x i8], ptr @{global}, i64 0, i64 0"
                ));
                Ok((result, Type::Str))
            }
            Expr::Number(text) => {
                if text.contains('.') {
                    let value: f64 = text.parse().map_err(|_| format!("invalid floating-point literal '{text}'"))?;
                    if !value.is_finite() {
                        return Err(format!("floating-point literal '{text}' is outside the supported finite range"));
                    }
                    Ok((format!("{value:.17e}"), Type::F64))
                } else {
                    let value: u128 = text.parse()
                        .map_err(|_| format!("integer literal '{text}' is outside the u64 range"))?;
                    let fits = |maximum: u128| value <= maximum;
                    let expected_signed_max = match expected {
                        Some(Type::I8) => Some(i8::MAX as u128),
                        Some(Type::I16) => Some(i16::MAX as u128),
                        Some(Type::I32) => Some(i32::MAX as u128),
                        Some(Type::I64) => Some(i64::MAX as u128),
                        _ => None,
                    };
                    if let Some(maximum) = expected_signed_max {
                        if fits(maximum) {
                            let ty = expected.unwrap();
                            Ok((value.to_string(), ty))
                        } else {
                            Err(format!("integer literal '{text}' does not fit {}", expected.unwrap().llvm()))
                        }
                    } else if expected == Some(Type::U8) {
                        if fits(u8::MAX as u128) { Ok((value.to_string(), Type::U8)) }
                        else { Err(format!("integer literal '{text}' does not fit u8")) }
                    } else if expected == Some(Type::U16) {
                        if fits(u16::MAX as u128) { Ok((value.to_string(), Type::U16)) }
                        else { Err(format!("integer literal '{text}' does not fit u16")) }
                    } else if expected == Some(Type::U32) {
                        if fits(u32::MAX as u128) { Ok((value.to_string(), Type::U32)) }
                        else { Err(format!("integer literal '{text}' does not fit u32")) }
                    } else if expected == Some(Type::U64) {
                        if fits(u64::MAX as u128) { Ok((value.to_string(), Type::U64)) }
                        else { Err(format!("integer literal '{text}' does not fit u64")) }
                    } else if fits(i32::MAX as u128) {
                        Ok((value.to_string(), Type::I32))
                    } else if fits(i64::MAX as u128) {
                        Ok((value.to_string(), Type::I64))
                    } else if fits(u64::MAX as u128) {
                        Ok((value.to_string(), Type::U64))
                    } else {
                        Err(format!("integer literal '{text}' is outside the u64 range"))
                    }
                }
            }
            Expr::Bool(value) => Ok((if *value { "true" } else { "false" }.to_owned(), Type::Bool)),
            Expr::Null => Ok(("null".to_owned(), Type::Ptr)),
            Expr::Variable(name) => {
                let binding = self.lookup(name).ok_or_else(|| format!("unknown variable '{name}'"))?;
                let result = self.fresh_value();
                self.emit(format!("{result} = load {}, ptr {}", binding.ty.llvm(), binding.slot));
                Ok((result, binding.ty))
            }
            Expr::Index { array, index } => {
                let (element_ptr, element) = self.emit_array_element_ptr(array, index)?;
                let result = self.fresh_value();
                self.emit(format!("{result} = load {}, ptr {element_ptr}", element.llvm()));
                Ok((result, element.ty()))
            }
            Expr::Cast { value, target } => {
                if *target == Type::Void {
                    return Err("cannot cast a value to void".to_owned());
                }
                let (source, source_type) = self.emit_expr_expected(value, None)?;
                if source_type == *target {
                    return Ok((source, *target));
                }
                if matches!((source_type, *target), (Type::Str, Type::Ptr) | (Type::Ptr, Type::Str))
                    || matches!((source_type, *target), (Type::Ptr, Type::Array(_, _)) | (Type::Array(_, _), Type::Ptr))
                {
                    return Ok((source, *target));
                }
                if source_type.is_integer() && *target == Type::Ptr {
                    let result = self.fresh_value();
                    self.emit(format!("{result} = inttoptr {} {source} to ptr", source_type.llvm()));
                    return Ok((result, Type::Ptr));
                }
                if matches!(source_type, Type::Ptr | Type::Str | Type::Array(_, _)) && target.is_integer() {
                    let result = self.fresh_value();
                    self.emit(format!("{result} = ptrtoint ptr {source} to {}", target.llvm()));
                    return Ok((result, *target));
                }

                if source_type == Type::Bool && target.is_integer() {
                    let result = self.fresh_value();
                    self.emit(format!("{result} = zext i1 {source} to {}", target.llvm()));
                    return Ok((result, *target));
                }
                if source_type.is_integer() && *target == Type::Bool {
                    let result = self.fresh_value();
                    self.emit(format!("{result} = icmp ne {} {source}, 0", source_type.llvm()));
                    return Ok((result, Type::Bool));
                }
                if source_type == Type::Bool && *target == Type::F64 {
                    let result = self.fresh_value();
                    self.emit(format!("{result} = uitofp i1 {source} to double"));
                    return Ok((result, Type::F64));
                }
                if source_type == Type::F64 && *target == Type::Bool {
                    let result = self.fresh_value();
                    self.emit(format!("{result} = fcmp une double {source}, 0.000000e+00"));
                    return Ok((result, Type::Bool));
                }

                if let (Some(source_width), Some(target_width)) =
                    (source_type.integer_width(), target.integer_width())
                {
                    let result = self.fresh_value();
                    if source_width == target_width {
                        return Ok((source, *target));
                    }
                    if source_width > target_width {
                        self.emit(format!("{result} = trunc {} {source} to {}", source_type.llvm(), target.llvm()));
                    } else if source_type.is_unsigned() {
                        self.emit(format!("{result} = zext {} {source} to {}", source_type.llvm(), target.llvm()));
                    } else {
                        self.emit(format!("{result} = sext {} {source} to {}", source_type.llvm(), target.llvm()));
                    }
                    return Ok((result, *target));
                }

                if source_type.is_integer() && *target == Type::F64 {
                    let result = self.fresh_value();
                    let instruction = if source_type.is_unsigned() {
                        "uitofp"
                    } else {
                        "sitofp"
                    };
                    self.emit(format!("{result} = {instruction} {} {source} to double", source_type.llvm()));
                    return Ok((result, Type::F64));
                }
                if source_type == Type::F64 && target.is_integer() {
                    let result = self.fresh_value();
                    let instruction = if target.is_unsigned() { "fptoui" } else { "fptosi" };
                    self.emit(format!("{result} = {instruction} double {source} to {}", target.llvm()));
                    return Ok((result, *target));
                }

                Err(format!("cannot cast {} to {}", source_type.llvm(), target.llvm()))
            }
            Expr::Unary { op, right } => {
                if op == "-" {
                    if let Expr::Number(text) = right.as_ref() {
                        if !text.contains('.') {
                            let magnitude: i128 = text.parse()
                                .map_err(|_| format!("integer literal '-{text}' is outside the supported range"))?;
                            let signed = magnitude.checked_neg()
                                .ok_or_else(|| format!("integer literal '-{text}' is outside the supported range"))?;
                            let ty = match expected {
                                Some(Type::I8) if (i8::MIN as i128..=i8::MAX as i128).contains(&signed) => Type::I8,
                                Some(Type::I16) if (i16::MIN as i128..=i16::MAX as i128).contains(&signed) => Type::I16,
                                Some(Type::I32) if (i32::MIN as i128..=i32::MAX as i128).contains(&signed) => Type::I32,
                                Some(Type::I64) if (i64::MIN as i128..=i64::MAX as i128).contains(&signed) => Type::I64,
                                Some(Type::I8) => return Err(format!("integer literal '-{text}' does not fit i8")),
                                Some(Type::I16) => return Err(format!("integer literal '-{text}' does not fit i16")),
                                Some(Type::I32) => return Err(format!("integer literal '-{text}' does not fit i32")),
                                Some(Type::I64) if !(i64::MIN as i128..=i64::MAX as i128).contains(&signed) =>
                                    return Err(format!("integer literal '-{text}' does not fit i64")),
                                Some(Type::U8 | Type::U16 | Type::U32 | Type::U64) =>
                                    return Err("negative integer literals cannot initialize unsigned types directly; use an explicit cast".to_owned()),
                                Some(_) => {
                                    if (i32::MIN as i128..=i32::MAX as i128).contains(&signed) { Type::I32 }
                                    else if (i64::MIN as i128..=i64::MAX as i128).contains(&signed) { Type::I64 }
                                    else { return Err(format!("integer literal '-{text}' is outside the supported range")); }
                                }
                                None => {
                                    if (i32::MIN as i128..=i32::MAX as i128).contains(&signed) { Type::I32 }
                                    else if (i64::MIN as i128..=i64::MAX as i128).contains(&signed) { Type::I64 }
                                    else { return Err(format!("integer literal '-{text}' is outside the supported range")); }
                                }
                            };
                            return Ok((signed.to_string(), ty));
                        }
                    }
                }
                let expected_operand = if op == "!" { Some(Type::Bool) } else { expected };
                let (value, ty) = self.emit_expr_expected(right, expected_operand)?;
                match op.as_str() {
                    "~" if ty.is_integer() => {
                        let result = self.fresh_value();
                        self.emit(format!("{result} = xor {} {value}, -1", ty.llvm()));
                        Ok((result, ty))
                    }
                    "-" if ty.is_numeric() => {
                        let result = self.fresh_value();
                        if ty == Type::F64 {
                            self.emit(format!("{result} = fneg double {value}"));
                        } else {
                            self.emit(format!("{result} = sub {} 0, {value}", ty.llvm()));
                        }
                        Ok((result, ty))
                    }
                    "!" if ty == Type::Bool => {
                        let result = self.fresh_value();
                        self.emit(format!("{result} = xor i1 {value}, true"));
                        Ok((result, Type::Bool))
                    }
                    "-" => Err(format!("unary '-' requires a number, found {}", ty.llvm())),
                    "!" => Err(format!("unary '!' requires bool, found {}", ty.llvm())),
                    "~" => Err(format!("unary '~' requires an integer, found {}", ty.llvm())),
                    _ => Err(format!("unsupported unary operator '{op}'")),
                }
            }
            Expr::Binary { left, op, right } => {
                if op == "&&" || op == "||" {
                    return self.emit_logical(left, op, right);
                }
                let arithmetic = matches!(op.as_str(), "+" | "-" | "*" | "/" | "%" | "&" | "|" | "^" | "<<" | ">>");
                let expected_lhs = if arithmetic && expected.is_some_and(Type::is_numeric) { expected } else { None };
                let (lhs, lhs_ty) = self.emit_expr_expected(left, expected_lhs)?;
                let (rhs, rhs_ty) = self.emit_expr_expected(right, Some(lhs_ty))?;
                self.require_same(lhs_ty, rhs_ty, &format!("operator '{op}'"))?;

                let result_ty = match op.as_str() {
                    "+" | "-" | "*" | "/" if lhs_ty.is_numeric() => lhs_ty,
                    "%" | "&" | "|" | "^" | "<<" | ">>" if lhs_ty.is_integer() => lhs_ty,
                    "==" | "!=" if lhs_ty != Type::Void => Type::Bool,
                    "<" | "<=" | ">" | ">=" if lhs_ty.is_numeric() => Type::Bool,
                    "+" | "-" | "*" | "/" => return Err(format!("operator '{op}' requires numeric operands")),
                    "<" | "<=" | ">" | ">=" => return Err(format!("operator '{op}' requires numeric operands")),
                    "&" | "|" | "^" | "<<" | ">>" => return Err(format!("operator '{op}' requires integer operands")),
                    _ => return Err(format!("unsupported binary operator '{op}'")),
                };

                let result = self.fresh_value();
                let instruction = match (lhs_ty, op.as_str()) {
                    (Type::F64, "+") => format!("{result} = fadd double {lhs}, {rhs}"),
                    (Type::F64, "-") => format!("{result} = fsub double {lhs}, {rhs}"),
                    (Type::F64, "*") => format!("{result} = fmul double {lhs}, {rhs}"),
                    (Type::F64, "/") => format!("{result} = fdiv double {lhs}, {rhs}"),
                    (Type::I8 | Type::I16 | Type::I32 | Type::I64 | Type::U8 | Type::U16 | Type::U32 | Type::U64, "+") => format!("{result} = add {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::I8 | Type::I16 | Type::I32 | Type::I64 | Type::U8 | Type::U16 | Type::U32 | Type::U64, "-") => format!("{result} = sub {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::I8 | Type::I16 | Type::I32 | Type::I64 | Type::U8 | Type::U16 | Type::U32 | Type::U64, "*") => format!("{result} = mul {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::I8 | Type::I16 | Type::I32 | Type::I64, "/") => format!("{result} = sdiv {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::U8 | Type::U16 | Type::U32 | Type::U64, "/") => format!("{result} = udiv {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::I8 | Type::I16 | Type::I32 | Type::I64, "%") => format!("{result} = srem {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::U8 | Type::U16 | Type::U32 | Type::U64, "%") => format!("{result} = urem {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::I8 | Type::I16 | Type::I32 | Type::I64 | Type::U8 | Type::U16 | Type::U32 | Type::U64, "&") => format!("{result} = and {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::I8 | Type::I16 | Type::I32 | Type::I64 | Type::U8 | Type::U16 | Type::U32 | Type::U64, "|") => format!("{result} = or {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::I8 | Type::I16 | Type::I32 | Type::I64 | Type::U8 | Type::U16 | Type::U32 | Type::U64, "^") => format!("{result} = xor {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::I8 | Type::I16 | Type::I32 | Type::I64 | Type::U8 | Type::U16 | Type::U32 | Type::U64, "<<") => format!("{result} = shl {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::I8 | Type::I16 | Type::I32 | Type::I64, ">>") => format!("{result} = ashr {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::U8 | Type::U16 | Type::U32 | Type::U64, ">>") => format!("{result} = lshr {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::F64, "==") => format!("{result} = fcmp oeq double {lhs}, {rhs}"),
                    (Type::F64, "!=") => format!("{result} = fcmp une double {lhs}, {rhs}"),
                    (Type::F64, "<") => format!("{result} = fcmp olt double {lhs}, {rhs}"),
                    (Type::F64, "<=") => format!("{result} = fcmp ole double {lhs}, {rhs}"),
                    (Type::F64, ">") => format!("{result} = fcmp ogt double {lhs}, {rhs}"),
                    (Type::F64, ">=") => format!("{result} = fcmp oge double {lhs}, {rhs}"),
                    (_, "==") => format!("{result} = icmp eq {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (_, "!=") => format!("{result} = icmp ne {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::I8 | Type::I16 | Type::I32 | Type::I64, "<") => format!("{result} = icmp slt {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::U8 | Type::U16 | Type::U32 | Type::U64, "<") => format!("{result} = icmp ult {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::I8 | Type::I16 | Type::I32 | Type::I64, "<=") => format!("{result} = icmp sle {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::U8 | Type::U16 | Type::U32 | Type::U64, "<=") => format!("{result} = icmp ule {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::I8 | Type::I16 | Type::I32 | Type::I64, ">") => format!("{result} = icmp sgt {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::U8 | Type::U16 | Type::U32 | Type::U64, ">") => format!("{result} = icmp ugt {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::I8 | Type::I16 | Type::I32 | Type::I64, ">=") => format!("{result} = icmp sge {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::U8 | Type::U16 | Type::U32 | Type::U64, ">=") => format!("{result} = icmp uge {} {lhs}, {rhs}", lhs_ty.llvm()),
                    _ => return Err(format!("operator '{op}' is not valid for {}", lhs_ty.llvm())),
                };
                self.emit(instruction);
                Ok((result, result_ty))
            }
            Expr::Call { name, args } => {
                if name == "print" {
                    if args.len() != 1 {
                        return Err("print expects exactly one argument".to_owned());
                    }
                    let (value, ty) = self.emit_expr(&args[0])?;
                    self.uses_printf = true;
                    match ty {
                        Type::I8 => {
                            let promoted = self.fresh_value();
                            self.emit(format!("{promoted} = sext i8 {value} to i32"));
                            self.emit(format!("call i32 (ptr, ...) @printf(ptr @.fmt.i32, i32 {promoted})"));
                        }
                        Type::I16 => {
                            let promoted = self.fresh_value();
                            self.emit(format!("{promoted} = sext i16 {value} to i32"));
                            self.emit(format!("call i32 (ptr, ...) @printf(ptr @.fmt.i32, i32 {promoted})"));
                        }
                        Type::U8 => {
                            let promoted = self.fresh_value();
                            self.emit(format!("{promoted} = zext i8 {value} to i32"));
                            self.emit(format!("call i32 (ptr, ...) @printf(ptr @.fmt.u32, i32 {promoted})"));
                        }
                        Type::U16 => {
                            let promoted = self.fresh_value();
                            self.emit(format!("{promoted} = zext i16 {value} to i32"));
                            self.emit(format!("call i32 (ptr, ...) @printf(ptr @.fmt.u32, i32 {promoted})"));
                        }
                        Type::I32 => self.emit(format!("call i32 (ptr, ...) @printf(ptr @.fmt.i32, i32 {value})")),
                        Type::I64 => self.emit(format!("call i32 (ptr, ...) @printf(ptr @.fmt.i64, i64 {value})")),
                        Type::U32 => self.emit(format!("call i32 (ptr, ...) @printf(ptr @.fmt.u32, i32 {value})")),
                        Type::U64 => self.emit(format!("call i32 (ptr, ...) @printf(ptr @.fmt.u64, i64 {value})")),
                        Type::F64 => self.emit(format!("call i32 (ptr, ...) @printf(ptr @.fmt.f64, double {value})")),
                        Type::Bool => {
                            let promoted = self.fresh_value();
                            self.emit(format!("{promoted} = zext i1 {value} to i32"));
                            self.emit(format!("call i32 (ptr, ...) @printf(ptr @.fmt.i32, i32 {promoted})"));
                        }
                        Type::Str => self.emit(format!("call i32 (ptr, ...) @printf(ptr @.fmt.str, ptr {value})")),
                        Type::Ptr => return Err("print does not accept raw pointers".to_owned()),
                        Type::Array(_, _) => return Err("print does not accept arrays directly".to_owned()),
                        Type::Void => return Err("print cannot print a void expression".to_owned()),
                    }
                    return Ok((String::new(), Type::Void));
                }

                if name == "alloc" {
                    if args.len() != 1 {
                        return Err("alloc expects one byte-count argument".to_owned());
                    }
                    let (size, size_type) = self.emit_expr_expected(&args[0], Some(Type::I64))?;
                    self.require_same(Type::I64, size_type, "alloc byte count")?;
                    let result = self.fresh_value();
                    self.emit(format!("{result} = call ptr @malloc(i64 {size})"));
                    return Ok((result, Type::Ptr));
                }

                if name == "free" {
                    if args.len() != 1 {
                        return Err("free expects exactly one ptr argument".to_owned());
                    }
                    let (pointer, pointer_type) = self.emit_expr_expected(&args[0], Some(Type::Ptr))?;
                    self.require_same(Type::Ptr, pointer_type, "free argument")?;
                    self.emit(format!("call void @free(ptr {pointer})"));
                    return Ok((String::new(), Type::Void));
                }

                if name == "ptr_offset" {
                    if args.len() != 2 {
                        return Err("ptr_offset expects a ptr and a byte offset".to_owned());
                    }
                    let (pointer, pointer_type) = self.emit_expr_expected(&args[0], Some(Type::Ptr))?;
                    self.require_same(Type::Ptr, pointer_type, "ptr_offset pointer")?;
                    let (offset, offset_type) = self.emit_expr_expected(&args[1], Some(Type::I64))?;
                    self.require_same(Type::I64, offset_type, "ptr_offset byte offset")?;
                    let result = self.fresh_value();
                    self.emit(format!("{result} = getelementptr i8, ptr {pointer}, i64 {offset}"));
                    return Ok((result, Type::Ptr));
                }

                let loaded_type = match name.as_str() {
                    "load_i8" => Some(Type::I8),
                    "load_i16" => Some(Type::I16),
                    "load_i32" => Some(Type::I32),
                    "load_i64" => Some(Type::I64),
                    "load_u8" => Some(Type::U8),
                    "load_u16" => Some(Type::U16),
                    "load_u32" => Some(Type::U32),
                    "load_u64" => Some(Type::U64),
                    "load_f64" => Some(Type::F64),
                    "load_bool" => Some(Type::Bool),
                    "load_ptr" => Some(Type::Ptr),
                    _ => None,
                };
                if let Some(loaded_type) = loaded_type {
                    if args.len() != 1 {
                        return Err(format!("{name} expects exactly one ptr argument"));
                    }
                    let (pointer, pointer_type) = self.emit_expr_expected(&args[0], Some(Type::Ptr))?;
                    self.require_same(Type::Ptr, pointer_type, &format!("{name} argument"))?;
                    let result = self.fresh_value();
                    self.emit(format!("{result} = load {}, ptr {pointer}", loaded_type.llvm()));
                    return Ok((result, loaded_type));
                }

                let stored_type = match name.as_str() {
                    "store_i8" => Some(Type::I8),
                    "store_i16" => Some(Type::I16),
                    "store_i32" => Some(Type::I32),
                    "store_i64" => Some(Type::I64),
                    "store_u8" => Some(Type::U8),
                    "store_u16" => Some(Type::U16),
                    "store_u32" => Some(Type::U32),
                    "store_u64" => Some(Type::U64),
                    "store_f64" => Some(Type::F64),
                    "store_bool" => Some(Type::Bool),
                    "store_ptr" => Some(Type::Ptr),
                    _ => None,
                };
                if let Some(stored_type) = stored_type {
                    if args.len() != 2 {
                        return Err(format!("{name} expects a ptr and a value"));
                    }
                    let (pointer, pointer_type) = self.emit_expr_expected(&args[0], Some(Type::Ptr))?;
                    self.require_same(Type::Ptr, pointer_type, &format!("{name} destination"))?;
                    let (value, value_type) = self.emit_expr_expected(&args[1], Some(stored_type))?;
                    self.require_same(stored_type, value_type, &format!("{name} value"))?;
                    self.emit(format!("store {} {value}, ptr {pointer}", stored_type.llvm()));
                    return Ok((String::new(), Type::Void));
                }

                let signature = self.signatures.get(name).cloned()
                    .ok_or_else(|| format!("unknown function '{name}'"))?;
                if signature.params.len() != args.len() {
                    return Err(format!("function '{name}' expects {} argument(s), got {}", signature.params.len(), args.len()));
                }
                let mut lowered = Vec::new();
                for (arg, expected) in args.iter().zip(signature.params.iter()) {
                    let (value, actual) = self.emit_expr_expected(arg, Some(*expected))?;
                    self.require_same(*expected, actual, &format!("argument to '{name}'"))?;
                    lowered.push(format!("{} {value}", actual.llvm()));
                }
                let arguments = lowered.join(", ");
                if signature.return_type == Type::Void {
                    self.emit(format!("call void @{name}({arguments})"));
                    Ok((String::new(), Type::Void))
                } else {
                    let result = self.fresh_value();
                    self.emit(format!("{result} = call {} @{name}({arguments})", signature.return_type.llvm()));
                    Ok((result, signature.return_type))
                }
            }
        }
    }

    fn emit_stmt(&mut self, statement: &Stmt) -> Result<(), String> {
        if self.terminated {
            return Ok(());
        }
        match statement {
            Stmt::Block(statements) => {
                self.scopes.push(HashMap::new());
                for statement in statements {
                    if self.terminated { break; }
                    self.emit_stmt(statement)?;
                }
                self.scopes.pop();
            }
            Stmt::Let { name, declared_type, mutable, value } => {
                if self.scopes.last().is_some_and(|scope| scope.contains_key(name)) {
                    return Err(format!("variable '{name}' is already declared in this scope"));
                }
                let (value, inferred_type) = self.emit_expr_expected(value, *declared_type)?;
                let ty = declared_type.unwrap_or(inferred_type);
                if ty == Type::Void {
                    return Err("cannot infer a variable from a void expression".to_owned());
                }
                self.require_same(ty, inferred_type, &format!("initializer for '{name}'"))?;
                let slot = self.fresh_slot(ty);
                self.emit(format!("store {} {value}, ptr {slot}", ty.llvm()));
                self.scopes.last_mut().unwrap().insert(name.clone(), Binding { ty, slot, mutable: *mutable });
            }
            Stmt::IndexAssign { array, index, value } => {
                let (element_ptr, element) = self.emit_array_element_ptr(array, index)?;
                let element_type = element.ty();
                let (stored_value, value_type) = self.emit_expr_expected(value, Some(element_type))?;
                self.require_same(element_type, value_type, "array element assignment")?;
                self.emit(format!("store {} {stored_value}, ptr {element_ptr}", element.llvm()));
            }
            Stmt::Assign { name, value } => {
                let binding = self.lookup(name).ok_or_else(|| format!("assignment to unknown variable '{name}'"))?;
                if !binding.mutable {
                    return Err(format!("cannot assign to immutable variable '{name}'; declare it with 'let mut'"));
                }
                let (value, ty) = self.emit_expr_expected(value, Some(binding.ty))?;
                self.require_same(binding.ty, ty, &format!("assignment to '{name}'"))?;
                self.emit(format!("store {} {value}, ptr {}", ty.llvm(), binding.slot));
            }
            Stmt::Return(value) => {
                match (self.return_type, value) {
                    (Type::Void, None) => self.emit("ret void"),
                    (Type::Void, Some(_)) => return Err("void function cannot return a value".to_owned()),
                    (_, None) => return Err(format!("function must return {}", self.return_type.llvm())),
                    (expected, Some(expression)) => {
                        let (value, actual) = self.emit_expr_expected(expression, Some(expected))?;
                        self.require_same(expected, actual, "return value")?;
                        self.emit(format!("ret {} {value}", expected.llvm()));
                    }
                }
                self.terminated = true;
            }
            Stmt::Break => {
                let (_, break_target) = self.loop_stack.last().cloned()
                    .ok_or_else(|| "'break' can only be used inside a loop".to_owned())?;
                self.emit(format!("br label %{break_target}"));
                self.terminated = true;
            }
            Stmt::Continue => {
                let (continue_target, _) = self.loop_stack.last().cloned()
                    .ok_or_else(|| "'continue' can only be used inside a loop".to_owned())?;
                self.emit(format!("br label %{continue_target}"));
                self.terminated = true;
            }
            Stmt::Expression(expression) => {
                self.emit_expr(expression)?;
            }
            Stmt::If { condition, then_branch, else_branch } => {
                let (condition_value, condition_type) = self.emit_expr(condition)?;
                self.require_same(Type::Bool, condition_type, "if condition")?;
                let then_label = self.fresh_label("if.then");
                let else_label = self.fresh_label("if.else");
                let end_label = self.fresh_label("if.end");
                self.emit(format!("br i1 {condition_value}, label %{then_label}, label %{else_label}"));
                self.terminated = true;

                self.label(&then_label);
                self.emit_stmt(then_branch)?;
                let then_terminated = self.terminated;
                if !then_terminated {
                    self.emit(format!("br label %{end_label}"));
                    self.terminated = true;
                }

                self.label(&else_label);
                if let Some(else_branch) = else_branch {
                    self.emit_stmt(else_branch)?;
                }
                let else_terminated = self.terminated;
                if !else_terminated {
                    self.emit(format!("br label %{end_label}"));
                    self.terminated = true;
                }

                if then_terminated && else_terminated {
                    self.terminated = true;
                } else {
                    self.label(&end_label);
                }
            }
            Stmt::For { initializer, condition, increment, body } => {
                self.scopes.push(HashMap::new());
                self.emit_stmt(initializer)?;

                let condition_label = self.fresh_label("for.cond");
                let body_label = self.fresh_label("for.body");
                let increment_label = self.fresh_label("for.step");
                let end_label = self.fresh_label("for.end");

                self.emit(format!("br label %{condition_label}"));
                self.terminated = true;

                self.label(&condition_label);
                let (condition_value, condition_type) = self.emit_expr(condition)?;
                self.require_same(Type::Bool, condition_type, "for condition")?;
                self.emit(format!("br i1 {condition_value}, label %{body_label}, label %{end_label}"));
                self.terminated = true;

                self.label(&body_label);
                self.loop_stack.push((increment_label.clone(), end_label.clone()));
                self.emit_stmt(body)?;
                self.loop_stack.pop();
                if !self.terminated {
                    self.emit(format!("br label %{increment_label}"));
                    self.terminated = true;
                }

                self.label(&increment_label);
                self.emit_stmt(increment)?;
                if !self.terminated {
                    self.emit(format!("br label %{condition_label}"));
                    self.terminated = true;
                }

                self.label(&end_label);
                self.scopes.pop();
            }
            Stmt::While { condition, body } => {
                let condition_label = self.fresh_label("while.cond");
                let body_label = self.fresh_label("while.body");
                let end_label = self.fresh_label("while.end");
                self.emit(format!("br label %{condition_label}"));
                self.terminated = true;

                self.label(&condition_label);
                let (condition_value, condition_type) = self.emit_expr(condition)?;
                self.require_same(Type::Bool, condition_type, "while condition")?;
                self.emit(format!("br i1 {condition_value}, label %{body_label}, label %{end_label}"));
                self.terminated = true;

                self.label(&body_label);
                self.loop_stack.push((condition_label.clone(), end_label.clone()));
                self.emit_stmt(body)?;
                self.loop_stack.pop();
                if !self.terminated {
                    self.emit(format!("br label %{condition_label}"));
                    self.terminated = true;
                }
                self.label(&end_label);
            }
        }
        Ok(())
    }

    fn generate(mut self, function: &Function) -> Result<(String, Vec<String>, bool), String> {
        for (index, (name, ty)) in function.params.iter().enumerate() {
            if self.scopes[0].contains_key(name) {
                return Err(format!("duplicate parameter '{name}'"));
            }
            let slot = self.fresh_slot(*ty);
            self.emit(format!("store {} %arg{index}, ptr {slot}", ty.llvm()));
            self.scopes[0].insert(name.clone(), Binding { ty: *ty, slot, mutable: false });
        }

        let body_statement = function.body.as_ref()
            .ok_or_else(|| format!("external function '{}' has no body to generate", function.name))?;
        self.emit_stmt(body_statement)?;
        if function.return_type != Type::Void && !self.terminated {
            return Err(format!("not every path in function '{}' returns {}", function.name, function.return_type.llvm()));
        }
        if function.return_type == Type::Void && !self.terminated {
            self.emit("ret void");
            self.terminated = true;
        }

        let mut body = String::from("entry:\n");
        for alloca in self.allocas {
            body.push_str(&alloca);
            body.push('\n');
        }
        for line in self.lines {
            body.push_str(&line);
            body.push('\n');
        }
        Ok((body, self.string_globals, self.uses_printf))
    }
}

fn statement_always_returns(statement: &Stmt) -> bool {
    match statement {
        Stmt::Return(_) => true,
        Stmt::Block(statements) => statements.iter().any(statement_always_returns),
        Stmt::If { then_branch, else_branch: Some(else_branch), .. } => {
            statement_always_returns(then_branch) && statement_always_returns(else_branch)
        }
        _ => false,
    }
}

fn compile_to_llvm(functions: &[Function]) -> Result<String, String> {
    let mut signatures = HashMap::new();
    for function in functions {
        let signature = Signature {
            params: function.params.iter().map(|(_, ty)| *ty).collect(),
            return_type: function.return_type,
        };
        if signatures.insert(function.name.clone(), signature).is_some() {
            return Err(format!("duplicate function '{}'", function.name));
        }
    }

    let main = signatures.get("main").ok_or_else(|| "program must define fn main() -> i32".to_owned())?;
    let main_function = functions.iter().find(|function| function.name == "main").unwrap();
    if main_function.external || !main.params.is_empty() || main.return_type != Type::I32 {
        return Err("entry point must be a definition with signature fn main() -> i32".to_owned());
    }
    for function in functions {
        if let Some(body) = &function.body {
            if function.return_type != Type::Void && !statement_always_returns(body) {
                return Err(format!("function '{}' must return a value on every path", function.name));
            }
        }
    }

    let mut module = String::from("; RamLang native module generated by ramcomp\nsource_filename = \"ramlang\"\n\n");
    module.push_str("@.fmt.i32 = private unnamed_addr constant [4 x i8] [i8 37, i8 100, i8 10, i8 0], align 1\n");
    module.push_str("@.fmt.i64 = private unnamed_addr constant [6 x i8] [i8 37, i8 108, i8 108, i8 100, i8 10, i8 0], align 1\n");
    module.push_str("@.fmt.u32 = private unnamed_addr constant [4 x i8] [i8 37, i8 117, i8 10, i8 0], align 1\n");
    module.push_str("@.fmt.u64 = private unnamed_addr constant [6 x i8] [i8 37, i8 108, i8 108, i8 117, i8 10, i8 0], align 1\n");
    module.push_str("@.fmt.f64 = private unnamed_addr constant [4 x i8] [i8 37, i8 103, i8 10, i8 0], align 1\n");
    module.push_str("@.fmt.str = private unnamed_addr constant [4 x i8] [i8 37, i8 115, i8 10, i8 0], align 1\n");
    module.push_str("declare i32 @printf(ptr, ...)\n\n");
    module.push_str("declare ptr @malloc(i64)\n");
    module.push_str("declare void @free(ptr)\n");
    module.push_str("declare void @llvm.trap() cold noreturn nounwind\n\n");

    for function in functions {
        if function.external {
            let params = function.params.iter()
                .map(|(_, ty)| ty.llvm())
                .collect::<Vec<_>>()
                .join(", ");
            module.push_str(&format!("declare {} @{}({params})\n\n", function.return_type.llvm(), function.name));
            continue;
        }

        let params = function.params.iter().enumerate()
            .map(|(index, (_, ty))| format!("{} %arg{index}", ty.llvm()))
            .collect::<Vec<_>>()
            .join(", ");
        let generator = FunctionGenerator::new(&signatures, function.return_type, &function.name);
        let (body, string_globals, _) = generator.generate(function)?;
        for global in string_globals {
            module.push_str(&global);
            module.push('\n');
        }
        let header = format!("define {} @{}({params}) {{\n", function.return_type.llvm(), function.name);
        module.push_str(&header);
        module.push_str(&body);
        module.push_str("}\n\n");
    }
    Ok(module)
}

fn parse_source(source: &str) -> Result<Vec<Function>, String> {
    let tokens = Lexer::new(source).lex()?;
    Parser::new(tokens).parse_program()
}

#[derive(Debug)]
struct Options {
    sources: Vec<PathBuf>,
    output: Option<PathBuf>,
    emit_llvm: bool,
    emit_object: bool,
    emit_assembly: bool,
    check_only: bool,
    link_libs: Vec<String>,
    link_search_paths: Vec<PathBuf>,
    link_args: Vec<String>,
    target: Option<String>,
    opt_level: String,
    opt_level_explicit: bool,
}

fn parse_args() -> Result<Options, String> {
    parse_args_from(env::args().skip(1))
}

fn parse_args_from<I>(arguments: I) -> Result<Options, String>
where
    I: IntoIterator<Item = String>,
{
    let mut args = arguments.into_iter().peekable();
    let Some(source) = args.next() else {
        return Err("usage: ramcomp <source.ram> [additional.ram ...] [--check] [--emit-llvm [output.ll] | --emit-obj | --emit-asm] [-o output] [--target TRIPLE] [--opt-level LEVEL] [--link-lib NAME] [--link-search PATH] [--link-arg ARG]".to_owned());
    };
    if source == "--help" || source == "-h" {
        println!("ramcomp - RamLang LLVM compiler\n\nUSAGE:\n  ramcomp <source.ram> [additional.ram ...] [--check] [--emit-llvm [output.ll]] [-o output] [--target TRIPLE] [--opt-level LEVEL] [--link-lib NAME] [--link-search PATH] [--link-arg ARG]\n\nOPTIONS:\n  --check                 Parse and type-check without producing an executable\n  --emit-llvm             Write LLVM IR instead of compiling a native executable\n  --emit-obj              Emit a native object file without linking\n  --emit-asm              Emit target assembly without linking\n  --target <triple>       Select the Clang compilation target\n  --opt-level <level>     Optimization level: 0, 1, 2, 3, s, or z (default: 2)\n  -o <path>               Set output executable or IR path\n  -l, --link-lib <name>   Link a native library (for example, --link-lib m)\n  -L, --link-search <dir> Add a native library search directory\n  --link-arg <arg>        Pass an additional argument to Clang\n  -h, --help              Show this help\n\nNative compilation uses clang (LLVM 15 or newer). Set RAMLANG_CLANG to override its path.");
        process::exit(0);
    }

    let mut options = Options {
        sources: vec![PathBuf::from(source)],
        output: None,
        emit_llvm: false,
        emit_object: false,
        emit_assembly: false,
        check_only: false,
        link_libs: Vec::new(),
        link_search_paths: Vec::new(),
        link_args: Vec::new(),
        target: None,
        opt_level: "2".to_owned(),
        opt_level_explicit: false,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--check" => options.check_only = true,
            "--emit-obj" | "--emit-object" => options.emit_object = true,
            "--emit-asm" => options.emit_assembly = true,
            "--emit-llvm" => {
                options.emit_llvm = true;
                if args.peek().is_some_and(|next| !next.starts_with('-')) {
                    options.output = args.next().map(PathBuf::from);
                }
            }
            "-o" => {
                let Some(path) = args.next() else { return Err("expected a path after -o".to_owned()); };
                options.output = Some(PathBuf::from(path));
            }
            "--target" => {
                let Some(target) = args.next() else { return Err("expected a target triple after --target".to_owned()); };
                if target.starts_with('-') || target.is_empty() {
                    return Err(format!("invalid target triple '{target}'"));
                }
                options.target = Some(target);
            }
            "--opt-level" => {
                let Some(level) = args.next() else { return Err("expected a level after --opt-level".to_owned()); };
                if !matches!(level.as_str(), "0" | "1" | "2" | "3" | "s" | "z") {
                    return Err(format!("invalid optimization level '{level}'; expected 0, 1, 2, 3, s, or z"));
                }
                options.opt_level = level;
                options.opt_level_explicit = true;
            }
            "--link-lib" | "-l" => {
                let Some(name) = args.next() else { return Err(format!("expected a library name after {arg}")); };
                if name.starts_with('-') || name.is_empty() {
                    return Err(format!("invalid library name '{name}'"));
                }
                options.link_libs.push(name);
            }
            "--link-search" | "-L" => {
                let Some(path) = args.next() else { return Err(format!("expected a directory after {arg}")); };
                options.link_search_paths.push(PathBuf::from(path));
            }
            "--link-arg" => {
                let Some(value) = args.next() else { return Err("expected an argument after --link-arg".to_owned()); };
                options.link_args.push(value);
            }
            _ if !arg.starts_with('-') => options.sources.push(PathBuf::from(arg)),
            _ => return Err(format!("unknown option '{arg}'")),
        }
    }
    let output_mode_count = usize::from(options.emit_llvm)
        + usize::from(options.emit_object)
        + usize::from(options.emit_assembly);
    if output_mode_count > 1 {
        return Err("--emit-llvm, --emit-obj, and --emit-asm are mutually exclusive".to_owned());
    }
    if options.check_only && output_mode_count > 0 {
        return Err("--check cannot be combined with an output mode".to_owned());
    }
    let link_options_present = !options.link_libs.is_empty()
        || !options.link_search_paths.is_empty()
        || !options.link_args.is_empty();
    if (options.check_only || options.emit_llvm) && (options.target.is_some() || options.opt_level_explicit) {
        return Err("target and optimization options require native code generation and cannot be combined with --check or --emit-llvm".to_owned());
    }
    if (options.check_only || output_mode_count > 0) && link_options_present {
        return Err("linker options require native linking and cannot be combined with --check or an emit-only mode".to_owned());
    }
    Ok(options)
}

fn compile(options: Options) -> Result<(), String> {
    let mut functions = Vec::new();
    for source_path in &options.sources {
        let source = fs::read_to_string(source_path)
            .map_err(|error| format!("cannot read {}: {error}", source_path.display()))?;
        let mut parsed = parse_source(&source)
            .map_err(|error| format!("{}: {error}", source_path.display()))?;
        functions.append(&mut parsed);
    }
    let source_label = options.sources.iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let llvm_ir = compile_to_llvm(&functions)
        .map_err(|error| format!("{source_label}: {error}"))?;

    if options.check_only {
        println!("{source_label}: OK");
        return Ok(());
    }

    if options.emit_llvm {
        let output = options.output.unwrap_or_else(|| options.sources[0].with_extension("ll"));
        fs::write(&output, llvm_ir)
            .map_err(|error| format!("cannot write {}: {error}", output.display()))?;
        println!("wrote {}", output.display());
        return Ok(());
    }

    let output = if let Some(output) = options.output {
        output
    } else if options.emit_object {
        options.sources[0].with_extension("o")
    } else if options.emit_assembly {
        options.sources[0].with_extension("s")
    } else {
        options.sources[0].with_extension("")
    };
    let ir_path = env::temp_dir().join(format!("ramlang-{}.ll", process::id()));
    fs::write(&ir_path, llvm_ir)
        .map_err(|error| format!("cannot write temporary LLVM IR: {error}"))?;
    let clang = env::var_os("RAMLANG_CLANG").unwrap_or_else(|| "clang".into());
    let mut command = Command::new(&clang);
    command.arg(format!("-O{}", options.opt_level)).arg("-Wno-override-module");
    if let Some(target) = &options.target {
        command.arg("-target").arg(target);
    }
    if options.emit_object {
        command.arg("-c");
    } else if options.emit_assembly {
        command.arg("-S");
    }
    command.arg(&ir_path);
    if !options.emit_object && !options.emit_assembly {
        for path in &options.link_search_paths {
            command.arg("-L").arg(path);
        }
        for library in &options.link_libs {
            command.arg(format!("-l{library}"));
        }
        for argument in &options.link_args {
            command.arg(argument);
        }
    }
    let result = command
        .arg("-o")
        .arg(&output)
        .output()
        .map_err(|error| format!("could not launch clang ({:?}): {error}; install LLVM/Clang 15+ or set RAMLANG_CLANG", clang));
    let _ = fs::remove_file(&ir_path);
    let result = result?;
    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        return Err(format!("clang failed to compile the generated LLVM IR:\n{stderr}\nInstall LLVM/Clang 15+ and verify that clang accepts opaque-pointer IR."));
    }
    println!("built {}", output.display());
    Ok(())
}

fn main() {
    match parse_args().and_then(compile) {
        Ok(()) => {}
        Err(error) => {
            eprintln!("ramcomp: {error}");
            process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(source: &str) -> Result<String, String> {
        let functions = parse_source(source)?;
        compile_to_llvm(&functions)
    }

    #[test]
    fn emits_integer_arithmetic() {
        let ir = compile("fn main() -> i32 { let x: i32 = 20 + 22; return x; }").unwrap();
        assert!(ir.contains("add i32 20, 22"));
        assert!(ir.contains("ret i32"));
    }

    #[test]
    fn parses_native_linker_options() {
        let options = parse_args_from([
            "example.ram".to_owned(),
            "--link-lib".to_owned(),
            "m".to_owned(),
            "--link-search".to_owned(),
            "/usr/lib".to_owned(),
            "--link-arg".to_owned(),
            "-Wl,--as-needed".to_owned(),
            "--target".to_owned(),
            "x86_64-unknown-linux-gnu".to_owned(),
            "--opt-level".to_owned(),
            "0".to_owned(),
            "-o".to_owned(),
            "example".to_owned(),
        ]).unwrap();
        assert_eq!(options.link_libs, vec!["m"]);
        assert_eq!(options.link_search_paths, vec![PathBuf::from("/usr/lib")]);
        assert_eq!(options.link_args, vec!["-Wl,--as-needed"]);
        assert_eq!(options.target.as_deref(), Some("x86_64-unknown-linux-gnu"));
        assert_eq!(options.opt_level, "0");
        assert!(options.opt_level_explicit);
        assert_eq!(options.output, Some(PathBuf::from("example")));
    }

    #[test]
    fn parses_multiple_source_files() {
        let options = parse_args_from([
            "main.ram".to_owned(),
            "math.ram".to_owned(),
            "runtime.ram".to_owned(),
            "-o".to_owned(),
            "program".to_owned(),
        ]).unwrap();
        assert_eq!(options.sources, vec![
            PathBuf::from("main.ram"),
            PathBuf::from("math.ram"),
            PathBuf::from("runtime.ram"),
        ]);
        assert_eq!(options.output, Some(PathBuf::from("program")));
    }

    #[test]
    fn parses_object_and_assembly_emission_modes() {
        let object = parse_args_from([
            "example.ram".to_owned(),
            "--emit-obj".to_owned(),
            "-o".to_owned(),
            "example.o".to_owned(),
            "--target".to_owned(),
            "x86_64-unknown-linux-gnu".to_owned(),
            "--opt-level".to_owned(),
            "0".to_owned(),
        ]).unwrap();
        assert!(object.emit_object);
        assert!(!object.emit_assembly);
        assert_eq!(object.output, Some(PathBuf::from("example.o")));
        assert_eq!(object.opt_level, "0");

        let assembly = parse_args_from([
            "example.ram".to_owned(),
            "--emit-asm".to_owned(),
        ]).unwrap();
        assert!(assembly.emit_assembly);
        assert_eq!(assembly.sources[0], PathBuf::from("example.ram"));
    }

    #[test]
    fn rejects_conflicting_emit_modes() {
        let error = parse_args_from([
            "example.ram".to_owned(),
            "--emit-obj".to_owned(),
            "--emit-asm".to_owned(),
        ]).unwrap_err();
        assert!(error.contains("mutually exclusive"));
    }

    #[test]
    fn rejects_linking_options_when_emitting_object() {
        let error = parse_args_from([
            "example.ram".to_owned(),
            "--emit-obj".to_owned(),
            "--link-lib".to_owned(),
            "m".to_owned(),
        ]).unwrap_err();
        assert!(error.contains("linker options require native linking"));
    }

    #[test]
    fn rejects_linker_options_for_ir_only_modes() {
        let error = parse_args_from([
            "example.ram".to_owned(),
            "--emit-llvm".to_owned(),
            "--link-lib".to_owned(),
            "m".to_owned(),
        ]).unwrap_err();
        assert!(error.contains("linker options require native linking"));
    }

    #[test]
    fn rejects_target_and_explicit_optimization_for_ir_only_modes() {
        let error = parse_args_from([
            "example.ram".to_owned(),
            "--check".to_owned(),
            "--opt-level".to_owned(),
            "2".to_owned(),
        ]).unwrap_err();
        assert!(error.contains("target and optimization options require native code generation"));
    }

    #[test]
    fn emits_if_else_control_flow() {
        let source = "fn main() -> i32 { let x: i32 = 3; if (x > 1) { return 1; } else { return 0; } }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("if.then"));
        assert!(ir.contains("icmp sgt i32"));
    }

    #[test]
    fn supports_mutable_variables_and_loops() {
        let source = "fn main() -> i32 { let mut x: i32 = 0; while (x < 4) { x = x + 1; } return x; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("while.cond"));
        assert!(ir.contains("store i32"));
    }

    #[test]
    fn supports_functions_and_print() {
        let source = "fn twice(x: i32) -> i32 { return x * 2; } fn main() -> i32 { print(twice(21)); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("define i32 @twice(i32 %arg0)"));
        assert!(ir.contains("@printf"));
    }

    #[test]
    fn rejects_immutable_assignment() {
        let source = "fn main() -> i32 { let x: i32 = 1; x = 2; return x; }";
        assert!(compile(source).unwrap_err().contains("immutable"));
    }

    #[test]
    fn rejects_type_mismatches() {
        let source = "fn main() -> i32 { let x: i32 = true; return x; }";
        assert!(compile(source).unwrap_err().contains("initializer"));
    }

    #[test]
    fn rejects_missing_return() {
        let source = "fn main() -> i32 { let x: i32 = 1; }";
        assert!(compile(source).unwrap_err().contains("return"));
    }

    #[test]
    fn parses_comments_and_precedence() {
        let source = "/* head */ fn main() -> i32 { // body\n return 1 + 2 * 3; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("mul i32"));
        assert!(ir.contains("add i32"));
    }

    #[test]
    fn uses_expected_type_for_integer_literals() {
        let source = "fn wide(x: i64) -> i64 { return x + 1; } fn main() -> i32 { let value: i64 = wide(41); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("add i64"));
        assert!(ir.contains("call i64 @wide(i64 41)"));
    }

    #[test]
    fn emits_floating_point_operations() {
        let source = "fn main() -> i32 { let value: f64 = 1.5 + 2.0; print(value); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("fadd double"));
        assert!(ir.contains("@.fmt.f64"));
    }

    #[test]
    fn emits_utf8_string_literals_and_escapes() {
        let source = r#"fn main() -> i32 { print("café\n\"ok\""); return 0; }"#;
        let ir = compile(source).unwrap();
        assert!(ir.contains("@.str.main.0"));
        assert!(ir.contains("\\C3\\A9"));
        assert!(ir.contains("\\0A"));
        assert!(ir.contains("@.fmt.str"));
    }

    #[test]
    fn rejects_unknown_string_escapes() {
        let error = parse_source(r#"fn main() -> i32 { print("bad\q"); return 0; }"#).unwrap_err();
        assert!(error.contains("unsupported string escape"));
    }

    #[test]
    fn emits_explicit_memory_operations() {
        let source = "fn main() -> i32 { let p: ptr = alloc(8); store_i32(p, 7); let q: ptr = ptr_offset(p, 4); store_i32(q, 42); print(load_i32(q)); free(p); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("call ptr @malloc(i64 8)"));
        assert!(ir.contains("store i32 42"));
        assert!(ir.contains("load i32, ptr"));
        assert!(ir.contains("call void @free(ptr"));
        assert!(ir.contains("getelementptr i8, ptr"));
    }

    #[test]
    fn lowers_fixed_array_reads_writes_and_bounds_checks() {
        let source = "fn main() -> i32 { let data: [i32; 4] = alloc(16) as [i32; 4]; data[0] = 10; data[3] = 42; print(data[3]); free(data as ptr); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("getelementptr inbounds i32"));
        assert!(ir.contains("icmp ult i64"));
        assert!(ir.contains("call void @llvm.trap()"));
        assert!(ir.contains("store i32 42"));
        assert!(ir.contains("array.bounds.trap"));
    }

    #[test]
    fn lowers_stack_backed_array_literals() {
        let source = "fn main() -> i32 { let values: [i32; 4] = [10, 20, 30, 42]; print(values[3]); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("alloca [4 x i32]"));
        assert!(ir.contains("store i32 20"));
        assert!(ir.contains("store i32 42"));
        assert!(ir.contains("getelementptr inbounds [4 x i32]"));
        assert!(ir.contains("call void @llvm.trap()"));
    }

    #[test]
    fn rejects_array_literal_length_and_element_type_mismatches() {
        let length_error = compile("fn main() -> i32 { let values: [i32; 2] = [1, 2, 3]; return 0; }").unwrap_err();
        assert!(length_error.contains("type requires 2"));
        let type_error = compile("fn main() -> i32 { let values: [i32; 2] = [1, true]; return 0; }").unwrap_err();
        assert!(type_error.contains("array literal element"));
    }

    #[test]
    fn rejects_non_integer_array_indices() {
        let source = "fn main() -> i32 { let data: [i32; 2] = alloc(8) as [i32; 2]; print(data[true]); free(data as ptr); return 0; }";
        assert!(compile(source).unwrap_err().contains("array index must be an integer"));
    }

    #[test]
    fn rejects_zero_length_and_void_element_arrays() {
        let zero = parse_source("fn main() -> i32 { let a: [i32; 0] = null as [i32; 0]; return 0; }").unwrap_err();
        assert!(zero.contains("array length must be greater than zero"));
        let void_element = parse_source("fn main() -> i32 { let a: [void; 2] = null as [void; 2]; return 0; }").unwrap_err();
        assert!(void_element.contains("array element type must be a non-void scalar"));
    }

    #[test]
    fn accepts_null_pointer_values() {
        let source = "fn main() -> i32 { let p: ptr = null; if (p == null) { return 0; } else { return 1; } }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("icmp eq ptr"));
    }

    #[test]
    fn emits_external_c_abi_declarations() {
        let source = "extern fn abs(value: i32) -> i32; fn main() -> i32 { return abs(-42) - 42; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("declare i32 @abs(i32)"));
        assert!(ir.contains("call i32 @abs(i32"));
    }

    #[test]
    fn emits_short_circuit_logic_and_remainder() {
        let source = "fn side() -> bool { print(9); return true; } fn main() -> i32 { let value: i32 = 7 % 4; if (false && side() || value == 3) { print(0); } else { print(1); } return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("phi i1"));
        assert!(ir.contains("srem i32 7, 4"));
    }

    #[test]
    fn emits_break_and_continue_branches() {
        let source = "fn main() -> i32 { let mut i: i32 = 0; while (true) { i = i + 1; if (i == 2) { continue; } if (i == 3) { break; } } print(i); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("while.cond"));
        assert!(ir.contains("while.end"));
    }

    #[test]
    fn emits_for_loop_control_flow() {
        let source = "fn main() -> i32 { let mut sum: i32 = 0; for (let mut i: i32 = 0; i < 10; i = i + 1) { if (i == 3) { continue; } if (i == 7) { break; } sum = sum + i; } print(sum); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("for.cond"));
        assert!(ir.contains("for.step"));
        assert!(ir.contains("for.end"));
    }

    #[test]
    fn for_loop_variable_is_scoped_to_the_loop() {
        let source = "fn main() -> i32 { for (let mut i: i32 = 0; i < 2; i = i + 1) { } return i; }";
        let error = compile(source).unwrap_err();
        assert!(error.contains("unknown variable 'i'"));
    }

    #[test]
    fn emits_bitwise_and_shift_instructions() {
        let source = "fn main() -> i32 { let value: i32 = (6 & 3) | (1 << 3); print(~0); print(value); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("and i32 6, 3"));
        assert!(ir.contains("shl i32 1, 3"));
        assert!(ir.contains("or i32"));
        assert!(ir.contains("xor i32 0, -1"));
    }

    #[test]
    fn accepts_signed_minimum_integer_literals() {
        let source = "fn main() -> i32 { let a: i8 = -128; let b: i16 = -32768; let c: i32 = -2147483648; let d: i64 = -9223372036854775808; print(a); print(b); print(c); print(d); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("store i8 -128"));
        assert!(ir.contains("store i16 -32768"));
        assert!(ir.contains("store i32 -2147483648"));
        assert!(ir.contains("store i64 -9223372036854775808"));
    }

    #[test]
    fn lowers_numeric_and_boolean_casts() {
        let source = "fn main() -> i32 { let a: i32 = 258.75 as i32; let b: i8 = a as i8; let c: i64 = b as i64; let d: f64 = c as f64; let flag: bool = b as bool; let truth: f64 = true as f64; print(a); print(b); print(c); print(d); print(flag); print(truth); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("fptosi double"));
        assert!(ir.contains("trunc i32"));
        assert!(ir.contains("sext i8"));
        assert!(ir.contains("sitofp i64"));
        assert!(ir.contains("icmp ne i8"));
        assert!(ir.contains("uitofp i1"));
    }

    #[test]
    fn lowers_unsigned_types_and_operations() {
        let source = "fn main() -> i32 { let a: u8 = 255; let b: u16 = 65535; let c: u32 = 4294967295; let d: u64 = 18446744073709551615; let gt: bool = a > 1 as u8; let quotient: u32 = c / 5; let shifted: u64 = d >> 63; let widened: u16 = a as u16; let as_float: f64 = d as f64; print(a); print(b); print(c); print(d); print(gt); print(quotient); print(shifted); print(widened); print(as_float); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("icmp ugt i8"));
        assert!(ir.contains("udiv i32"));
        assert!(ir.contains("lshr i64"));
        assert!(ir.contains("zext i8"));
        assert!(ir.contains("uitofp i64"));
        assert!(ir.contains("@.fmt.u64"));
    }

    #[test]
    fn rejects_negative_literal_for_unsigned_initializer() {
        let error = compile("fn main() -> i32 { let x: u8 = -1; return 0; }").unwrap_err();
        assert!(error.contains("negative integer literals cannot initialize unsigned types"));
    }

    #[test]
    fn rejects_casts_to_void_and_float_pointer_casts() {
        let void_cast = compile("fn main() -> i32 { let x: i32 = 1 as void; return 0; }").unwrap_err();
        assert!(void_cast.contains("cast a value to void"));
        let pointer_cast = compile("fn main() -> i32 { let x: ptr = 1.0 as ptr; return 0; }").unwrap_err();
        assert!(pointer_cast.contains("cannot cast double to ptr"));
    }

    #[test]
    fn lowers_explicit_integer_pointer_casts() {
        let source = "fn main() -> i32 { let p: ptr = alloc(4); let address: u64 = p as u64; let q: ptr = address as ptr; store_i32(q, 42); print(load_i32(p)); free(p); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("ptrtoint ptr"));
        assert!(ir.contains("inttoptr i64"));
    }

    #[test]
    fn rejects_break_outside_loops() {
        let source = "fn main() -> i32 { break; return 0; }";
        assert!(compile(source).unwrap_err().contains("only be used inside a loop"));
    }
}
