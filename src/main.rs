mod llvm_backend;

use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    process::{self, Command},
};

fn is_float_literal(text: &str) -> bool {
    text.contains('.') || text.contains('e') || text.contains('E')
}

// LLVM's hexadecimal float literal spelling uses the 64-bit IEEE representation
// of the exactly widened value, so f32 constants round-trip without decimal loss.
fn llvm_f32_constant(value: f32) -> String {
    format!("0x{:016X}", (value as f64).to_bits())
}

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
                if matches!(self.peek(0), Some(b'e' | b'E')) {
                    self.advance();
                    if matches!(self.peek(0), Some(b'+' | b'-')) {
                        self.advance();
                    }
                    if !self.peek(0).is_some_and(|b| b.is_ascii_digit()) {
                        return Err(format!("{line}:{column}: floating-point exponent requires at least one digit"));
                    }
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

            if byte == b'.' && self.peek(1) == Some(b'.') && self.peek(2) == Some(b'.') {
                self.advance();
                self.advance();
                self.advance();
                tokens.push(self.token(TokenKind::Symbol("...".to_owned()), "...".to_owned(), line, column));
                continue;
            }

            let pair = match (byte, self.peek(1)) {
                (b'&', Some(b'&')) => Some("&&"),
                (b'|', Some(b'|')) => Some("||"),
                (b'<', Some(b'<')) => Some("<<"),
                (b'>', Some(b'>')) => Some(">>"),
                (b'-', Some(b'>')) => Some("->"),
                (b'=', Some(b'=')) => Some("=="),
                (b'=', Some(b'>')) => Some("=>"),
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
    F32,
    F64,
    Bool,
    Ptr,
    Str,
    Alias(u64),
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
            Type::F32 => Some(Self::F32),
            Type::F64 => Some(Self::F64),
            Type::Bool => Some(Self::Bool),
            Type::Ptr => Some(Self::Ptr),
            Type::Str => Some(Self::Str),
            Type::Struct(type_id) => Some(Self::Alias(type_id)),
            _ => None,
        }
    }

    fn from_resolved_type(ty: Type) -> Option<Self> {
        match ty {
            Type::I8 => Some(Self::I8),
            Type::I16 => Some(Self::I16),
            Type::I32 | Type::Enum(_) => Some(Self::I32),
            Type::I64 => Some(Self::I64),
            Type::U8 => Some(Self::U8),
            Type::U16 => Some(Self::U16),
            Type::U32 => Some(Self::U32),
            Type::U64 => Some(Self::U64),
            Type::F32 => Some(Self::F32),
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
            Self::F32 => Type::F32,
            Self::F64 => Type::F64,
            Self::Bool => Type::Bool,
            Self::Ptr => Type::Ptr,
            Self::Str => Type::Str,
            Self::Alias(type_id) => Type::Struct(type_id),
        }
    }

    fn llvm(self) -> &'static str {
        self.ty().llvm()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FutureOutput {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    Bool,
    Str,
    Ptr,
    Void,
    Enum(u64),
    Named(u64),
}

impl FutureOutput {
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
            Type::F32 => Some(Self::F32),
            Type::F64 => Some(Self::F64),
            Type::Bool => Some(Self::Bool),
            Type::Str => Some(Self::Str),
            Type::Ptr => Some(Self::Ptr),
            Type::Void => Some(Self::Void),
            Type::Enum(id) => Some(Self::Enum(id)),
            Type::Struct(id) => Some(Self::Named(id)),
            Type::Array(_, _) | Type::Future(_) => None,
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
            Self::F32 => Type::F32,
            Self::F64 => Type::F64,
            Self::Bool => Type::Bool,
            Self::Str => Type::Str,
            Self::Ptr => Type::Ptr,
            Self::Void => Type::Void,
            Self::Enum(id) => Type::Enum(id),
            Self::Named(id) => Type::Struct(id),
        }
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
    F32,
    F64,
    Bool,
    Str,
    Ptr,
    Future(FutureOutput),
    Array(ArrayElement, u32),
    Struct(u64),
    Enum(u64),
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
            Self::F32 => "float",
            Self::F64 => "double",
            Self::Bool => "i1",
            Self::Str => "ptr",
            Self::Ptr | Self::Future(_) => "ptr",
            Self::Array(_, _) => "ptr",
            Self::Struct(_) => "ptr",
            Self::Enum(_) => "i32",
            Self::Void => "void",
        }
    }

    fn is_numeric(self) -> bool {
        matches!(self, Self::I8 | Self::I16 | Self::I32 | Self::I64 | Self::U8 | Self::U16 | Self::U32 | Self::U64 | Self::F32 | Self::F64 | Self::Enum(_))
    }

    fn is_float(self) -> bool {
        matches!(self, Self::F32 | Self::F64)
    }

    fn is_integer(self) -> bool {
        matches!(self, Self::I8 | Self::I16 | Self::I32 | Self::I64 | Self::U8 | Self::U16 | Self::U32 | Self::U64 | Self::Enum(_))
    }

    fn is_unsigned(self) -> bool {
        matches!(self, Self::U8 | Self::U16 | Self::U32 | Self::U64)
    }

    fn integer_width(self) -> Option<u32> {
        match self {
            Self::I8 | Self::U8 => Some(8),
            Self::I16 | Self::U16 => Some(16),
            Self::I32 | Self::U32 | Self::Enum(_) => Some(32),
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
    variadic: bool,
    asynchronous: bool,
}

#[derive(Debug, Clone)]
struct StructDef {
    id: u64,
    name: String,
    fields: Vec<(String, Type)>,
    packed: bool,
}

#[derive(Debug, Clone)]
struct EnumDef {
    id: u64,
    name: String,
    variants: Vec<(String, i32)>,
}

#[derive(Debug, Clone)]
struct TypeAlias {
    id: u64,
    name: String,
    target: Type,
}

#[derive(Debug, Clone)]
struct GlobalConst {
    name: String,
    ty: Type,
    value: Expr,
}

#[derive(Debug, Clone)]
struct GlobalVar {
    name: String,
    ty: Type,
    mutable: bool,
    value: Option<Expr>,
    external: bool,
}

#[derive(Debug, Clone, Default)]
struct Program {
    functions: Vec<Function>,
    structs: Vec<StructDef>,
    enums: Vec<EnumDef>,
    type_aliases: Vec<TypeAlias>,
    constants: Vec<GlobalConst>,
    static_vars: Vec<GlobalVar>,
    imports: Vec<String>,
    type_references: HashMap<u64, String>,
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
    Match { value: Expr, arms: Vec<(Expr, Stmt)>, default: Option<Box<Stmt>> },
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
    Match { value: Box<Expr>, arms: Vec<(Expr, Expr)>, default: Option<Box<Expr>> },
    SizeOf(Type),
    AlignOf(Type),
    OffsetOf(Type, String),
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
    type_references: HashMap<u64, String>,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, index: 0, type_references: HashMap::new() }
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
            if self.match_keyword("import") {
                if !matches!(self.current().kind, TokenKind::String) {
                    return Err(self.error("import expects a quoted relative source path".to_owned()));
                }
                let path = self.current().text.clone();
                self.index += 1;
                if path.is_empty() {
                    return Err(self.error("import path cannot be empty".to_owned()));
                }
                self.expect_symbol(";")?;
                program.imports.push(path);
                continue;
            }
            if self.match_keyword("packed") {
                if !self.match_keyword("struct") {
                    return Err(self.error("expected 'struct' after 'packed'".to_owned()));
                }
                program.structs.push(self.parse_struct(true)?);
                continue;
            }
            if self.match_keyword("struct") {
                program.structs.push(self.parse_struct(false)?);
                continue;
            }
            if self.match_keyword("enum") {
                program.enums.push(self.parse_enum()?);
                continue;
            }
            if self.match_keyword("type") {
                program.type_aliases.push(self.parse_type_alias()?);
                continue;
            }
            if self.match_keyword("const") {
                program.constants.push(self.parse_global_const()?);
                continue;
            }
            if self.match_keyword("static") {
                program.static_vars.push(self.parse_global_var(false)?);
                continue;
            }
            let asynchronous = self.match_keyword("async");
            let external = self.match_keyword("extern");
            if asynchronous && external {
                return Err(self.error("'async extern fn' is not supported; async functions must have a RamLang body".to_owned()));
            }
            if external && self.match_keyword("static") {
                if asynchronous {
                    return Err(self.error("'async extern static' is invalid".to_owned()));
                }
                program.static_vars.push(self.parse_global_var(true)?);
                continue;
            }
            if !self.match_keyword("fn") {
                return Err(self.error("expected an import, struct, enum, type alias, const, static, function declaration, 'extern static', 'extern fn', or 'async fn'".to_owned()));
            }
            program.functions.push(self.parse_function(external, asynchronous)?);
        }
        if program.functions.is_empty() && program.structs.is_empty() && program.enums.is_empty()
            && program.type_aliases.is_empty() && program.constants.is_empty()
            && program.static_vars.is_empty() && program.imports.is_empty()
        {
            return Err("source file does not contain any declarations".to_owned());
        }
        program.type_references = std::mem::take(&mut self.type_references);
        Ok(program)
    }

    fn parse_global_var(&mut self, external: bool) -> Result<GlobalVar, String> {
        let mutable = self.match_keyword("mut");
        let name = self.expect_ident("static variable name")?;
        self.expect_symbol(":")?;
        let ty = self.parse_type()?;
        if ty == Type::Void {
            return Err(self.error("a static variable cannot have type void".to_owned()));
        }
        if external && matches!(ty, Type::Array(_, _) | Type::Struct(_)) {
            return Err(self.error("extern static currently supports scalar and raw-pointer types only".to_owned()));
        }
        let value = if external {
            None
        } else {
            self.expect_symbol("=")?;
            Some(self.parse_expression()?)
        };
        self.expect_symbol(";")?;
        Ok(GlobalVar { name, ty, mutable, value, external })
    }

    fn parse_type_alias(&mut self) -> Result<TypeAlias, String> {
        let name = self.expect_ident("type alias name")?;
        self.expect_symbol("=")?;
        let target = self.parse_type()?;
        self.expect_symbol(";")?;
        Ok(TypeAlias { id: struct_type_id(&name), name, target })
    }

    fn parse_global_const(&mut self) -> Result<GlobalConst, String> {
        let name = self.expect_ident("constant name")?;
        self.expect_symbol(":")?;
        let ty = self.parse_type()?;
        if ty == Type::Void {
            return Err(self.error("a global constant cannot have type void".to_owned()));
        }
        self.expect_symbol("=")?;
        let value = self.parse_expression()?;
        self.expect_symbol(";")?;
        Ok(GlobalConst { name, ty, value })
    }

    fn parse_enum(&mut self) -> Result<EnumDef, String> {
        let name = self.expect_ident("enum name")?;
        let id = struct_type_id(&name);
        self.type_references.insert(id, name.clone());
        self.expect_symbol("{")?;
        let mut variants = Vec::new();
        let mut next_value: Option<i32> = Some(0);
        while !self.check_symbol("}") && !self.at_end() {
            let variant = self.expect_ident("enum variant name")?;
            if variants.iter().any(|(existing, _)| existing == &variant) {
                return Err(self.error(format!("duplicate variant '{variant}' in enum '{name}'")));
            }
            let explicit_value = self.match_symbol("=");
            let value = if explicit_value {
                let negative = self.match_symbol("-");
                if !matches!(self.current().kind, TokenKind::Number) || is_float_literal(&self.current().text) {
                    return Err(self.error("enum discriminant must be an integer literal".to_owned()));
                }
                let text = self.current().text.clone();
                self.index += 1;
                let parsed: i64 = text.parse()
                    .map_err(|_| self.error("enum discriminant is outside the i32 range".to_owned()))?;
                let parsed = if negative { -parsed } else { parsed };
                i32::try_from(parsed)
                    .map_err(|_| self.error("enum discriminant is outside the i32 range".to_owned()))?
            } else {
                next_value.ok_or_else(|| self.error("enum discriminant overflow before implicit variant".to_owned()))?
            };
            next_value = value.checked_add(1);
            variants.push((variant, value));
            if !self.match_symbol(",") {
                break;
            }
        }
        self.expect_symbol("}")?;
        if variants.is_empty() {
            return Err(self.error(format!("enum '{name}' must contain at least one variant")));
        }
        Ok(EnumDef { id, name, variants })
    }

    fn parse_struct(&mut self, packed: bool) -> Result<StructDef, String> {
        let name = self.expect_ident("struct name")?;
        let id = struct_type_id(&name);
        self.type_references.insert(id, name.clone());
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
        Ok(StructDef { id, name, fields, packed })
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
            "f32" => Ok(Type::F32),
            "f64" => Ok(Type::F64),
            "bool" => Ok(Type::Bool),
            "str" => Ok(Type::Str),
            "ptr" => Ok(Type::Ptr),
            "Future" => {
                self.expect_symbol("<")?;
                let output = self.parse_type()?;
                self.expect_symbol(">")?;
                let output = FutureOutput::from_type(output)
                    .ok_or_else(|| self.error("Future output must be a scalar, pointer, string, enum, or void type".to_owned()))?;
                Ok(Type::Future(output))
            }
            "void" => Ok(Type::Void),
            _ => {
                let id = struct_type_id(&name);
                self.type_references.insert(id, name);
                Ok(Type::Struct(id))
            },
        }
    }

    fn parse_function(&mut self, external: bool, asynchronous: bool) -> Result<Function, String> {
        let name = self.expect_ident("function name")?;
        self.expect_symbol("(")?;
        let mut params = Vec::new();
        let mut variadic = false;
        if !self.check_symbol(")") {
            loop {
                if self.match_symbol("...") {
                    variadic = true;
                    break;
                }
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
        if variadic && !external {
            return Err(self.error("variadic functions must be declared with 'extern fn'".to_owned()));
        }
        if asynchronous && variadic {
            return Err(self.error("async functions cannot be variadic".to_owned()));
        }
        if variadic && params.is_empty() {
            return Err(self.error("variadic extern functions must declare at least one fixed parameter".to_owned()));
        }
        let return_type = if self.match_symbol("->") { self.parse_type()? } else { Type::Void };
        let body = if external {
            self.expect_symbol(";")?;
            None
        } else {
            Some(self.parse_block()?)
        };
        Ok(Function { name, params, return_type, body, external, variadic, asynchronous })
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
        if self.match_keyword("match") {
            self.expect_symbol("(")?;
            let value = self.parse_expression()?;
            self.expect_symbol(")")?;
            self.expect_symbol("{")?;
            let mut arms = Vec::new();
            let mut default = None;
            while !self.check_symbol("}") && !self.at_end() {
                if matches!(self.current().kind, TokenKind::Ident)
                    && self.current().text == "_"
                    && self.tokens.get(self.index + 1).is_some_and(|token| token.kind == TokenKind::Symbol("=>".to_owned()))
                {
                    self.index += 1;
                    self.expect_symbol("=>")?;
                    if default.is_some() {
                        return Err(self.error("match statement contains more than one '_' fallback arm".to_owned()));
                    }
                    default = Some(Box::new(self.parse_block()?));
                } else {
                    let pattern = self.parse_expression()?;
                    self.expect_symbol("=>")?;
                    let body = self.parse_block()?;
                    arms.push((pattern, body));
                }
                self.match_symbol(",");
            }
            self.expect_symbol("}")?;
            if arms.is_empty() && default.is_none() {
                return Err(self.error("match statement must contain at least one arm".to_owned()));
            }
            return Ok(Stmt::Match { value, arms, default });
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
            && self.tokens.get(self.index + 1).is_some_and(|t| t.kind == TokenKind::Symbol(".".to_owned()))
        {
            let saved_index = self.index;
            let name = self.current().text.clone();
            self.index += 2;
            let field = self.expect_ident("field name after '.'")?;
            if self.match_symbol("=") {
                let value = self.parse_expression()?;
                self.expect_symbol(";")?;
                return Ok(Stmt::FieldAssign { base: Expr::Variable(name), field, value });
            }
            self.index = saved_index;
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
        if self.check_symbol("-") || self.check_symbol("!") || self.check_symbol("~") || self.check_symbol("&") {
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
            } else if self.match_symbol(".") {
                let field = self.expect_ident("field name after '.'")?;
                value = Expr::Field { base: Box::new(value), field };
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
        if self.match_keyword("await") {
            let operand = self.parse_unary()?;
            return Ok(Expr::Call {
                name: "__ramlang_await".to_owned(),
                args: vec![operand],
            });
        }
        if self.match_keyword("match") {
            self.expect_symbol("(")?;
            let value = Box::new(self.parse_expression()?);
            self.expect_symbol(")")?;
            self.expect_symbol("{")?;
            let mut arms = Vec::new();
            let mut default = None;
            while !self.check_symbol("}") && !self.at_end() {
                if matches!(self.current().kind, TokenKind::Ident)
                    && self.current().text == "_"
                    && self.tokens.get(self.index + 1).is_some_and(|token| token.kind == TokenKind::Symbol("=>".to_owned()))
                {
                    self.index += 1;
                    self.expect_symbol("=>")?;
                    if default.is_some() {
                        return Err(self.error("match expression contains more than one '_' fallback arm".to_owned()));
                    }
                    default = Some(Box::new(self.parse_expression()?));
                } else {
                    let pattern = self.parse_expression()?;
                    self.expect_symbol("=>")?;
                    let result = self.parse_expression()?;
                    arms.push((pattern, result));
                }
                self.match_symbol(",");
            }
            self.expect_symbol("}")?;
            if arms.is_empty() && default.is_none() {
                return Err(self.error("match expression must contain at least one arm".to_owned()));
            }
            return Ok(Expr::Match { value, arms, default });
        }
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
            if self.match_symbol("{") {
                let mut fields = Vec::new();
                if !self.check_symbol("}") {
                    loop {
                        let field_name = self.expect_ident("field name in struct literal")?;
                        self.expect_symbol(":")?;
                        let value = self.parse_expression()?;
                        fields.push((field_name, value));
                        if !self.match_symbol(",") {
                            break;
                        }
                        if self.check_symbol("}") {
                            break;
                        }
                    }
                }
                self.expect_symbol("}")?;
                let type_id = struct_type_id(&name);
                self.type_references.insert(type_id, name);
                return Ok(Expr::StructLiteral { type_id, fields });
            }
            if self.match_symbol("(") {
                if matches!(name.as_str(), "size_of" | "sizeof" | "align_of" | "alignof") {
                    let target_type = self.parse_type()?;
                    self.expect_symbol(")")?;
                    return Ok(if matches!(name.as_str(), "align_of" | "alignof") {
                        Expr::AlignOf(target_type)
                    } else {
                        Expr::SizeOf(target_type)
                    });
                }
                if matches!(name.as_str(), "offset_of" | "offsetof") {
                    let target_type = self.parse_type()?;
                    self.expect_symbol(",")?;
                    let field = self.expect_ident("field name for offset_of")?;
                    self.expect_symbol(")")?;
                    return Ok(Expr::OffsetOf(target_type, field));
                }
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
    variadic: bool,
    asynchronous: bool,
}

#[derive(Debug, Clone)]
struct Binding {
    ty: Type,
    slot: String,
    mutable: bool,
    read_only_pointee: bool,
}

struct FunctionGenerator<'a> {
    signatures: &'a HashMap<String, Signature>,
    structs: &'a HashMap<u64, StructDef>,
    enums: &'a HashMap<u64, EnumDef>,
    globals: &'a HashMap<String, (Type, String)>,
    global_bindings: &'a HashMap<String, Binding>,
    target_pointer_width: u32,
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
    fn new(
        signatures: &'a HashMap<String, Signature>,
        structs: &'a HashMap<u64, StructDef>,
        enums: &'a HashMap<u64, EnumDef>,
        globals: &'a HashMap<String, (Type, String)>,
        global_bindings: &'a HashMap<String, Binding>,
        target_pointer_width: u32,
        return_type: Type,
        function_name: &str,
    ) -> Self {
        Self {
            signatures, structs, enums, globals, global_bindings, target_pointer_width, return_type, allocas: Vec::new(), lines: Vec::new(),
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
            .or_else(|| self.global_bindings.get(name).cloned())
    }

    fn require_same(&self, expected: Type, actual: Type, context: &str) -> Result<(), String> {
        if expected == actual {
            Ok(())
        } else {
            Err(format!("{context}: expected {}, found {}", expected.llvm(), actual.llvm()))
        }
    }

    fn emit_lvalue_ptr(&mut self, expression: &Expr) -> Result<(String, Type), String> {
        match expression {
            Expr::Variable(name) => {
                let binding = self.lookup(name).ok_or_else(|| format!("unknown variable '{name}'"))?;
                Ok((binding.slot, binding.ty))
            }
            Expr::Index { array, index } => {
                let (element_ptr, element) = self.emit_array_element_ptr(array, index)?;
                Ok((element_ptr, element.ty()))
            }
            Expr::Field { base, field } => {
                let (field_ptr, field_type, _) = self.emit_struct_field_ptr(base, field)?;
                Ok((field_ptr, field_type))
            },
            _ => Err("address-of requires a variable, array element, or struct field".to_owned()),
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

    fn emit_struct_field_ptr(&mut self, base: &Expr, field: &str) -> Result<(String, Type, bool), String> {
        let (base_value, base_type) = self.emit_expr(base)?;
        let Type::Struct(type_id) = base_type else {
            return Err(format!("field access '.{field}' requires a struct value"));
        };
        let structure = self.structs.get(&type_id).cloned()
            .ok_or_else(|| format!("unknown struct type id {type_id:016x}"))?;
        let Some((field_index, (_, field_type))) = structure.fields.iter().enumerate()
            .find(|(_, (field_name, _))| field_name == field)
        else {
            return Err(format!("struct '{}' has no field '{field}'", structure.name));
        };
        let field_ptr = self.fresh_value();
        self.emit(format!(
            "{field_ptr} = getelementptr inbounds %struct.{type_id:016x}, ptr {base_value}, i32 0, i32 {field_index}"
        ));
        Ok((field_ptr, *field_type, structure.packed))
    }

    fn size_t_llvm_type(&self) -> &'static str {
        if self.target_pointer_width == 32 { "i32" } else { "i64" }
    }

    fn emit_size_t_count(&mut self, value: &str, context: &str) -> Result<String, String> {
        if self.target_pointer_width == 64 {
            return Ok(value.to_owned());
        }
        let nonnegative = self.fresh_value();
        self.emit(format!("{nonnegative} = icmp sge i64 {value}, 0"));
        let fits = self.fresh_value();
        self.emit(format!("{fits} = icmp ule i64 {value}, 4294967295"));
        let valid = self.fresh_value();
        self.emit(format!("{valid} = and i1 {nonnegative}, {fits}"));
        let ok_label = self.fresh_label("size_t.ok");
        let trap_label = self.fresh_label("size_t.trap");
        self.emit(format!("br i1 {valid}, label %{ok_label}, label %{trap_label}"));
        self.terminated = true;
        self.label(&trap_label);
        self.emit("call void @llvm.trap()");
        self.emit("unreachable");
        self.terminated = true;
        self.label(&ok_label);
        let converted = self.fresh_value();
        self.emit(format!("{converted} = trunc i64 {value} to i32"));
        let _ = context;
        Ok(converted)
    }

    fn widen_size_t_to_i64(&mut self, value: &str) -> String {
        if self.target_pointer_width == 64 {
            return value.to_owned();
        }
        let widened = self.fresh_value();
        self.emit(format!("{widened} = zext i32 {value} to i64"));
        widened
    }

    fn emit_shift_guard(&mut self, rhs: &str, ty: Type) -> Result<(), String> {
        let Some(width) = ty.integer_width() else {
            return Err("shift guard requires an integer type".to_owned());
        };
        let llvm_type = ty.llvm();
        let below_width = self.fresh_value();
        let relation = if ty.is_unsigned() { "ult" } else { "slt" };
        self.emit(format!("{below_width} = icmp {relation} {llvm_type} {rhs}, {width}"));
        let valid = if ty.is_unsigned() {
            below_width
        } else {
            let nonnegative = self.fresh_value();
            self.emit(format!("{nonnegative} = icmp sge {llvm_type} {rhs}, 0"));
            let both = self.fresh_value();
            self.emit(format!("{both} = and i1 {nonnegative}, {below_width}"));
            both
        };

        let ok_label = self.fresh_label("shift.ok");
        let trap_label = self.fresh_label("shift.trap");
        self.emit(format!("br i1 {valid}, label %{ok_label}, label %{trap_label}"));
        self.terminated = true;

        self.label(&trap_label);
        self.emit("call void @llvm.trap()");
        self.emit("unreachable");
        self.terminated = true;

        self.label(&ok_label);
        Ok(())
    }

    fn emit_integer_division_guard(&mut self, lhs: &str, rhs: &str, ty: Type) -> Result<(), String> {
        let Some(width) = ty.integer_width() else {
            return Err("division guard requires an integer type".to_owned());
        };
        let llvm_type = ty.llvm();
        let nonzero = self.fresh_value();
        self.emit(format!("{nonzero} = icmp ne {llvm_type} {rhs}, 0"));

        let valid = if ty.is_unsigned() {
            nonzero
        } else {
            let minimum = match width {
                8 => "-128",
                16 => "-32768",
                32 => "-2147483648",
                64 => "-9223372036854775808",
                _ => return Err(format!("unsupported signed division width {width}")),
            };
            let is_minimum = self.fresh_value();
            self.emit(format!("{is_minimum} = icmp eq {llvm_type} {lhs}, {minimum}"));
            let is_negative_one = self.fresh_value();
            self.emit(format!("{is_negative_one} = icmp eq {llvm_type} {rhs}, -1"));
            let overflow = self.fresh_value();
            self.emit(format!("{overflow} = and i1 {is_minimum}, {is_negative_one}"));
            let no_overflow = self.fresh_value();
            self.emit(format!("{no_overflow} = xor i1 {overflow}, true"));
            let valid = self.fresh_value();
            self.emit(format!("{valid} = and i1 {nonzero}, {no_overflow}"));
            valid
        };

        let ok_label = self.fresh_label("integer.division.ok");
        let trap_label = self.fresh_label("integer.division.trap");
        self.emit(format!("br i1 {valid}, label %{ok_label}, label %{trap_label}"));
        self.terminated = true;

        self.label(&trap_label);
        self.emit("call void @llvm.trap()");
        self.emit("unreachable");
        self.terminated = true;

        self.label(&ok_label);
        Ok(())
    }

    fn emit_raw_pointer_expr(&mut self, expression: &Expr, context: &str) -> Result<String, String> {
        let (value, ty) = self.emit_expr(expression)?;
        if matches!(ty, Type::Ptr | Type::Str | Type::Array(_, _) | Type::Struct(_)) {
            Ok(value)
        } else {
            Err(format!("{context} requires a pointer-like value, found {}", ty.llvm()))
        }
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

    fn emit_match_expr(
        &mut self,
        value: &Expr,
        arms: &[(Expr, Expr)],
        default: Option<&Expr>,
        expected: Option<Type>,
    ) -> Result<(String, Type), String> {
        let (scrutinee, scrutinee_type) = self.emit_expr(value)?;
        if !scrutinee_type.is_integer() && scrutinee_type != Type::Bool {
            return Err("match expression requires an integer, bool, or enum value".to_owned());
        }

        if default.is_none() {
            if scrutinee_type == Type::Bool {
                let mut covered = HashSet::<bool>::new();
                for (pattern, _) in arms {
                    let Expr::Bool(value) = pattern else {
                        return Err("exhaustive bool match expression requires true and false literal patterns or a '_' fallback".to_owned());
                    };
                    if !covered.insert(*value) {
                        return Err(format!("duplicate boolean pattern '{value}' in match expression"));
                    }
                }
                if covered.len() != 2 {
                    let missing = if !covered.contains(&true) { "true" } else { "false" };
                    return Err(format!("non-exhaustive match expression on bool; missing pattern: {missing}"));
                }
            } else {
                let Type::Enum(enum_id) = scrutinee_type else {
                    return Err("match expressions on non-enum values require a '_' fallback arm".to_owned());
                };
                let enumeration = self.enums.get(&enum_id)
                    .ok_or_else(|| format!("unknown enum type id {enum_id:016x}"))?;
                let required = enumeration.variants.iter().map(|(_, value)| *value).collect::<HashSet<_>>();
                let mut covered = HashSet::<i32>::new();
                for (pattern, _) in arms {
                    let Some((pattern_enum_id, discriminant)) = self.enum_variant_identity(pattern) else {
                        return Err(format!("match on enum '{}' requires named enum variants or a '_' fallback", enumeration.name));
                    };
                    if pattern_enum_id != enum_id {
                        return Err(format!("match arm belongs to a different enum than '{}'", enumeration.name));
                    }
                    if !covered.insert(discriminant) {
                        return Err(format!("duplicate discriminant {discriminant} in match on enum '{}'", enumeration.name));
                    }
                }
                if covered != required {
                    let missing = enumeration.variants.iter()
                        .filter(|(_, value)| !covered.contains(value))
                        .map(|(name, _)| name.as_str())
                        .collect::<Vec<_>>();
                    return Err(format!(
                        "non-exhaustive match expression on enum '{}'; missing variant(s): {}",
                        enumeration.name,
                        missing.join(", ")
                    ));
                }
            }
        }

        // Constant patterns can use an LLVM switch rather than emitting a
        // chain of comparisons. Keep dynamic patterns on the ordered path below.
        let mut switch_values = Vec::<String>::new();
        let mut switch_possible = true;
        let mut unique_switch_values = HashSet::<String>::new();
        for (pattern, _) in arms {
            let case_value = if let Type::Enum(enum_id) = scrutinee_type {
                match self.enum_variant_identity(pattern) {
                    Some((pattern_enum_id, discriminant)) if pattern_enum_id == enum_id => {
                        Some(discriminant.to_string())
                    }
                    Some(_) => {
                        switch_possible = false;
                        None
                    }
                    None => None,
                }
            } else if scrutinee_type == Type::Bool {
                match pattern {
                    Expr::Bool(value) => Some(if *value { "true" } else { "false" }.to_owned()),
                    _ => None,
                }
            } else if scrutinee_type.is_integer() {
                constant_integer_value(pattern)?.map(|value| value.to_string())
            } else {
                None
            };

            let Some(case_value) = case_value else {
                switch_possible = false;
                break;
            };
            let (_, pattern_type) = self.emit_expr_expected(pattern, Some(scrutinee_type))?;
            self.require_same(scrutinee_type, pattern_type, "match expression pattern")?;
            if !unique_switch_values.insert(case_value.clone()) {
                switch_possible = false;
                break;
            }
            switch_values.push(case_value);
        }

        if switch_possible {
            let end_label = self.fresh_label("match.expr.end");
            let arm_labels = arms.iter()
                .map(|_| self.fresh_label("match.expr.arm"))
                .collect::<Vec<_>>();
            let fallback_label = self.fresh_label(if default.is_some() {
                "match.expr.default"
            } else {
                "match.expr.trap"
            });
            let mut switch = format!(
                "switch {} {scrutinee}, label %{fallback_label} [",
                scrutinee_type.llvm()
            );
            for (case_value, arm_label) in switch_values.iter().zip(&arm_labels) {
                switch.push_str(&format!(
                    "\n    {} {case_value}, label %{arm_label}",
                    scrutinee_type.llvm()
                ));
            }
            switch.push_str("\n  ]");
            self.emit(switch);
            self.terminated = true;

            let mut result_type = expected;
            let mut incoming = Vec::<(String, String)>::new();
            for ((_, result_expression), arm_label) in arms.iter().zip(&arm_labels) {
                self.label(arm_label);
                let (result_value, arm_type) = self.emit_expr_expected(result_expression, result_type)?;
                if self.terminated {
                    continue;
                }
                if arm_type == Type::Void {
                    return Err("match expression arms must produce a value unless they diverge".to_owned());
                }
                if let Some(required_type) = result_type {
                    self.require_same(required_type, arm_type, "match expression arm")?;
                } else {
                    result_type = Some(arm_type);
                }
                let predecessor = self.current_block.clone();
                self.emit(format!("br label %{end_label}"));
                self.terminated = true;
                incoming.push((result_value, predecessor));
            }

            self.label(&fallback_label);
            if let Some(default_expression) = default {
                let (result_value, arm_type) = self.emit_expr_expected(default_expression, result_type)?;
                if !self.terminated {
                    if arm_type == Type::Void {
                        return Err("match expression fallback must produce a value unless it diverges".to_owned());
                    }
                    if let Some(required_type) = result_type {
                        self.require_same(required_type, arm_type, "match expression fallback")?;
                    } else {
                        result_type = Some(arm_type);
                    }
                    let predecessor = self.current_block.clone();
                    self.emit(format!("br label %{end_label}"));
                    self.terminated = true;
                    incoming.push((result_value, predecessor));
                }
            } else {
                self.emit("call void @llvm.trap()");
                self.emit("unreachable");
                self.terminated = true;
            }

            if incoming.is_empty() {
                return Err("match expression has no value-producing path".to_owned());
            }
            let result_type = result_type
                .ok_or_else(|| "cannot infer match expression result type".to_owned())?;
            self.label(&end_label);
            let result = self.fresh_value();
            let values = incoming.iter()
                .map(|(value, predecessor)| format!("[ {value}, %{predecessor} ]"))
                .collect::<Vec<_>>()
                .join(", ");
            self.emit(format!("{result} = phi {} {values}", result_type.llvm()));
            return Ok((result, result_type));
        }

        let end_label = self.fresh_label("match.expr.end");
        let mut result_type = expected;
        let mut incoming = Vec::<(String, String)>::new();

        for (pattern, result_expression) in arms {
            let (pattern_value, pattern_type) =
                self.emit_expr_expected(pattern, Some(scrutinee_type))?;
            self.require_same(scrutinee_type, pattern_type, "match expression pattern")?;
            let equal = self.fresh_value();
            self.emit(format!(
                "{equal} = icmp eq {} {scrutinee}, {pattern_value}",
                scrutinee_type.llvm()
            ));
            let arm_label = self.fresh_label("match.expr.arm");
            let next_label = self.fresh_label("match.expr.next");
            self.emit(format!("br i1 {equal}, label %{arm_label}, label %{next_label}"));
            self.terminated = true;

            self.label(&arm_label);
            let (result_value, arm_type) = self.emit_expr_expected(result_expression, result_type)?;
            if self.terminated {
                // A diverging expression such as panic() contributes no incoming
                // edge to the result phi, but it must not make the other arms fail.
                self.label(&next_label);
                continue;
            }
            if arm_type == Type::Void {
                return Err("match expression arms must produce a value unless they diverge".to_owned());
            }
            if let Some(required_type) = result_type {
                self.require_same(required_type, arm_type, "match expression arm")?;
            } else {
                result_type = Some(arm_type);
            }
            let predecessor = self.current_block.clone();
            self.emit(format!("br label %{end_label}"));
            self.terminated = true;
            incoming.push((result_value, predecessor));

            self.label(&next_label);
        }

        if let Some(default_expression) = default {
            let (result_value, arm_type) = self.emit_expr_expected(default_expression, result_type)?;
            if !self.terminated {
                if arm_type == Type::Void {
                    return Err("match expression fallback must produce a value unless it diverges".to_owned());
                }
                if let Some(required_type) = result_type {
                    self.require_same(required_type, arm_type, "match expression fallback")?;
                } else {
                    result_type = Some(arm_type);
                }
                let predecessor = self.current_block.clone();
                self.emit(format!("br label %{end_label}"));
                self.terminated = true;
                incoming.push((result_value, predecessor));
            }
        } else {
            self.emit("call void @llvm.trap()");
            self.emit("unreachable");
            self.terminated = true;
        }

        if incoming.is_empty() {
            return Err("match expression has no value-producing path".to_owned());
        }
        let result_type = result_type.ok_or_else(|| "cannot infer match expression result type".to_owned())?;
        self.label(&end_label);
        let result = self.fresh_value();
        let values = incoming.iter()
            .map(|(value, predecessor)| format!("[ {value}, %{predecessor} ]"))
            .collect::<Vec<_>>()
            .join(", ");
        self.emit(format!("{result} = phi {} {values}", result_type.llvm()));
        Ok((result, result_type))
    }

    fn emit_expr_expected(&mut self, expression: &Expr, expected: Option<Type>) -> Result<(String, Type), String> {
        match expression {
            Expr::Match { value, arms, default } => {
                self.emit_match_expr(value, arms, default.as_deref(), expected)
            }
            Expr::OffsetOf(ty, field) => {
                let Type::Struct(type_id) = ty else {
                    return Err("offset_of requires a struct type".to_owned());
                };
                let structure = self.structs.get(type_id)
                    .ok_or_else(|| format!("unknown struct type id {type_id:016x}"))?;
                let Some((field_index, _)) = structure.fields.iter().enumerate()
                    .find(|(_, (field_name, _))| field_name == field)
                else {
                    return Err(format!("struct '{}' has no field '{field}'", structure.name));
                };
                let address = self.fresh_value();
                self.emit(format!(
                    "{address} = getelementptr %struct.{type_id:016x}, ptr null, i32 0, i32 {field_index}"
                ));
                let offset = self.fresh_value();
                self.emit(format!("{offset} = ptrtoint ptr {address} to i64"));
                Ok((offset, Type::I64))
            }
            Expr::AlignOf(ty) => {
                if *ty == Type::Void {
                    return Err("align_of(void) is invalid".to_owned());
                }
                let abi_type = match ty {
                    Type::Array(element, length) => format!("[{length} x {}]", element.llvm()),
                    Type::Struct(type_id) => format!("%struct.{type_id:016x}"),
                    _ => ty.llvm().to_owned(),
                };
                let member_ptr = self.fresh_value();
                self.emit(format!(
                    "{member_ptr} = getelementptr {{ i8, {abi_type} }}, ptr null, i32 0, i32 1"
                ));
                let alignment = self.fresh_value();
                self.emit(format!("{alignment} = ptrtoint ptr {member_ptr} to i64"));
                Ok((alignment, Type::I64))
            }
            Expr::SizeOf(ty) => {
                match ty {
                    Type::Void => Err("size_of(void) is invalid".to_owned()),
                    Type::I8 | Type::U8 | Type::Bool => Ok(("1".to_owned(), Type::I64)),
                    Type::I16 | Type::U16 => Ok(("2".to_owned(), Type::I64)),
                    Type::I32 | Type::U32 | Type::F32 | Type::Enum(_) => Ok(("4".to_owned(), Type::I64)),
                    Type::I64 | Type::U64 | Type::F64 => Ok(("8".to_owned(), Type::I64)),
                    Type::Array(element, length) => {
                        let element_size = match element {
                            ArrayElement::I8 | ArrayElement::U8 | ArrayElement::Bool => Some(1_u64),
                            ArrayElement::I16 | ArrayElement::U16 => Some(2_u64),
                            ArrayElement::I32 | ArrayElement::U32 | ArrayElement::F32 => Some(4_u64),
                            ArrayElement::I64 | ArrayElement::U64 | ArrayElement::F64 => Some(8_u64),
                            ArrayElement::Ptr | ArrayElement::Str => None,
                            ArrayElement::Alias(type_id) => return Err(format!("unresolved array element alias {type_id:016x} in size_of")),
                        };
                        if let Some(element_size) = element_size {
                            Ok(((element_size * u64::from(*length)).to_string(), Type::I64))
                        } else {
                            let one_past = self.fresh_value();
                            self.emit(format!("{one_past} = getelementptr ptr, ptr null, i64 1"));
                            let pointer_size = self.fresh_value();
                            self.emit(format!("{pointer_size} = ptrtoint ptr {one_past} to i64"));
                            if *length == 1 {
                                Ok((pointer_size, Type::I64))
                            } else {
                                let size = self.fresh_value();
                                self.emit(format!("{size} = mul i64 {pointer_size}, {}", length));
                                Ok((size, Type::I64))
                            }
                        }
                    }
                    Type::Ptr | Type::Str | Type::Future(_) => {
                        let one_past = self.fresh_value();
                        self.emit(format!("{one_past} = getelementptr ptr, ptr null, i64 1"));
                        let size = self.fresh_value();
                        self.emit(format!("{size} = ptrtoint ptr {one_past} to i64"));
                        Ok((size, Type::I64))
                    }
                    Type::Struct(type_id) => {
                        if !self.structs.contains_key(type_id) {
                            return Err(format!("unknown struct type id {type_id:016x}"));
                        }
                        let one_past = self.fresh_value();
                        self.emit(format!(
                            "{one_past} = getelementptr %struct.{type_id:016x}, ptr null, i64 1"
                        ));
                        let size = self.fresh_value();
                        self.emit(format!("{size} = ptrtoint ptr {one_past} to i64"));
                        Ok((size, Type::I64))
                    }
                }
            }
            Expr::StructLiteral { type_id, fields } => {
                let structure = self.structs.get(type_id).cloned()
                    .ok_or_else(|| format!("unknown struct type id {type_id:016x}"))?;
                let declared_names = structure.fields.iter().map(|(name, _)| name.as_str()).collect::<HashSet<_>>();
                let mut supplied = HashMap::<String, &Expr>::new();
                for (field_name, expression) in fields {
                    if !declared_names.contains(field_name.as_str()) {
                        return Err(format!("struct '{}' has no field '{field_name}'", structure.name));
                    }
                    if supplied.insert(field_name.clone(), expression).is_some() {
                        return Err(format!("field '{field_name}' is initialized more than once in struct '{}'", structure.name));
                    }
                }
                if supplied.len() != structure.fields.len() {
                    let missing = structure.fields.iter()
                        .find(|(field_name, _)| !supplied.contains_key(field_name))
                        .map(|(field_name, _)| field_name.as_str())
                        .unwrap_or("unknown");
                    return Err(format!("struct literal for '{}' is missing field '{missing}'", structure.name));
                }

                let ir_type = format!("%struct.{type_id:016x}");
                let storage = self.fresh_value();
                self.allocas.push(format!("  {storage} = alloca {ir_type}"));
                for (field_index, (field_name, field_type)) in structure.fields.iter().enumerate() {
                    let expression = supplied.get(field_name).expect("validated struct field set");
                    let (value, actual_type) = self.emit_expr_expected(expression, Some(*field_type))?;
                    self.require_same(*field_type, actual_type, &format!("initializer for field '{field_name}'"))?;
                    let field_ptr = self.fresh_value();
                    self.emit(format!(
                        "{field_ptr} = getelementptr inbounds {ir_type}, ptr {storage}, i32 0, i32 {field_index}"
                    ));
                    if structure.packed {
                        self.emit(format!("store {} {value}, ptr {field_ptr}, align 1", field_type.llvm()));
                    } else {
                        self.emit(format!("store {} {value}, ptr {field_ptr}", field_type.llvm()));
                    }
                }
                Ok((storage, Type::Struct(*type_id)))
            }
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
                if is_float_literal(text) {
                    let value: f64 = text.parse().map_err(|_| format!("invalid floating-point literal '{text}'"))?;
                    if !value.is_finite() {
                        return Err(format!("floating-point literal '{text}' is outside the supported finite range"));
                    }
                    if expected == Some(Type::F32) {
                        let single = value as f32;
                        if !single.is_finite() {
                            return Err(format!("floating-point literal '{text}' is outside the f32 finite range"));
                        }
                        Ok((llvm_f32_constant(single), Type::F32))
                    } else {
                        Ok((format!("{value:.17e}"), Type::F64))
                    }
                } else {
                    let value: u128 = text.parse()
                        .map_err(|_| format!("integer literal '{text}' is outside the u64 range"))?;
                    if matches!(expected, Some(Type::F32 | Type::F64)) {
                        let value = value as f64;
                        if expected == Some(Type::F32) {
                            let single = value as f32;
                            if !single.is_finite() {
                                return Err(format!("integer literal '{text}' is outside the f32 finite range"));
                            }
                            return Ok((llvm_f32_constant(single), Type::F32));
                        }
                        return Ok((format!("{value:.17e}"), Type::F64));
                    }
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
                if let Some(binding) = self.lookup(name) {
                    let result = self.fresh_value();
                    self.emit(format!("{result} = load {}, ptr {}", binding.ty.llvm(), binding.slot));
                    Ok((result, binding.ty))
                } else if let Some((ty, value)) = self.globals.get(name) {
                    Ok((value.clone(), *ty))
                } else {
                    Err(format!("unknown variable '{name}'"))
                }
            }
            Expr::Index { array, index } => {
                let (element_ptr, element) = self.emit_array_element_ptr(array, index)?;
                let result = self.fresh_value();
                self.emit(format!("{result} = load {}, ptr {element_ptr}", element.llvm()));
                Ok((result, element.ty()))
            }
            Expr::Field { base, field } => {
                if let Expr::Variable(enum_name) = base.as_ref() {
                    if let Some(enumeration) = self.enums.get(&struct_type_id(enum_name)) {
                        let Some((_, value)) = enumeration.variants.iter().find(|(variant, _)| variant == field) else {
                            return Err(format!("enum '{}' has no variant '{}'", enumeration.name, field));
                        };
                        return Ok((value.to_string(), Type::Enum(enumeration.id)));
                    }
                }
                let (field_ptr, field_type, packed) = self.emit_struct_field_ptr(base, field)?;
                let result = self.fresh_value();
                if packed {
                    self.emit(format!("{result} = load {}, ptr {field_ptr}, align 1", field_type.llvm()));
                } else {
                    self.emit(format!("{result} = load {}, ptr {field_ptr}", field_type.llvm()));
                }
                Ok((result, field_type))
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
                    || matches!((source_type, *target), (Type::Ptr, Type::Struct(_)) | (Type::Struct(_), Type::Ptr))
                {
                    return Ok((source, *target));
                }
                if source_type.is_integer() && *target == Type::Ptr {
                    let result = self.fresh_value();
                    self.emit(format!("{result} = inttoptr {} {source} to ptr", source_type.llvm()));
                    return Ok((result, Type::Ptr));
                }
                if matches!(source_type, Type::Ptr | Type::Str | Type::Array(_, _) | Type::Struct(_)) && target.is_integer() {
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
                if source_type == Type::Bool && target.is_float() {
                    let result = self.fresh_value();
                    self.emit(format!("{result} = uitofp i1 {source} to {}", target.llvm()));
                    return Ok((result, *target));
                }
                if source_type.is_float() && *target == Type::Bool {
                    let result = self.fresh_value();
                    self.emit(format!("{result} = fcmp une {} {source}, 0.000000e+00", source_type.llvm()));
                    return Ok((result, Type::Bool));
                }
                if source_type.is_float() && target.is_float() {
                    let result = self.fresh_value();
                    match (source_type, *target) {
                        (Type::F32, Type::F64) => self.emit(format!("{result} = fpext float {source} to double")),
                        (Type::F64, Type::F32) => self.emit(format!("{result} = fptrunc double {source} to float")),
                        _ => return Ok((source, *target)),
                    }
                    return Ok((result, *target));
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

                if source_type.is_integer() && target.is_float() {
                    let result = self.fresh_value();
                    let instruction = if source_type.is_unsigned() { "uitofp" } else { "sitofp" };
                    self.emit(format!("{result} = {instruction} {} {source} to {}", source_type.llvm(), target.llvm()));
                    return Ok((result, *target));
                }
                if source_type.is_float() && target.is_integer() {
                    let result = self.fresh_value();
                    let instruction = if target.is_unsigned() { "fptoui" } else { "fptosi" };
                    self.emit(format!("{result} = {instruction} {} {source} to {}", source_type.llvm(), target.llvm()));
                    return Ok((result, *target));
                }

                Err(format!("cannot cast {} to {}", source_type.llvm(), target.llvm()))
            }
            Expr::Unary { op, right } => {
                if op == "&" {
                    let (address, _) = self.emit_lvalue_ptr(right)?;
                    return Ok((address, Type::Ptr));
                }
                if op == "-" {
                    if let Expr::Number(text) = right.as_ref() {
                        if !is_float_literal(text) {
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
                        if ty.is_float() {
                            self.emit(format!("{result} = fneg {} {value}", ty.llvm()));
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
                    "&" => Err("address-of requires a variable, array element, or struct field".to_owned()),
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

                if matches!(op.as_str(), "/" | "%")
                    && matches!(lhs_ty, Type::I8 | Type::I16 | Type::I32 | Type::I64 | Type::U8 | Type::U16 | Type::U32 | Type::U64)
                {
                    self.emit_integer_division_guard(&lhs, &rhs, lhs_ty)?;
                }
                if matches!(op.as_str(), "<<" | ">>")
                    && matches!(lhs_ty, Type::I8 | Type::I16 | Type::I32 | Type::I64 | Type::U8 | Type::U16 | Type::U32 | Type::U64)
                {
                    self.emit_shift_guard(&rhs, lhs_ty)?;
                }

                let result = self.fresh_value();
                let instruction = match (lhs_ty, op.as_str()) {
                    (Type::F32 | Type::F64, "+") => format!("{result} = fadd {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::F32 | Type::F64, "-") => format!("{result} = fsub {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::F32 | Type::F64, "*") => format!("{result} = fmul {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::F32 | Type::F64, "/") => format!("{result} = fdiv {} {lhs}, {rhs}", lhs_ty.llvm()),
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
                    (Type::F32 | Type::F64, "==") => format!("{result} = fcmp oeq {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::F32 | Type::F64, "!=") => format!("{result} = fcmp une {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::F32 | Type::F64, "<") => format!("{result} = fcmp olt {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::F32 | Type::F64, "<=") => format!("{result} = fcmp ole {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::F32 | Type::F64, ">") => format!("{result} = fcmp ogt {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::F32 | Type::F64, ">=") => format!("{result} = fcmp oge {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (_, "==") => format!("{result} = icmp eq {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (_, "!=") => format!("{result} = icmp ne {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::I8 | Type::I16 | Type::I32 | Type::I64 | Type::Enum(_), "<") => format!("{result} = icmp slt {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::U8 | Type::U16 | Type::U32 | Type::U64, "<") => format!("{result} = icmp ult {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::I8 | Type::I16 | Type::I32 | Type::I64 | Type::Enum(_), "<=") => format!("{result} = icmp sle {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::U8 | Type::U16 | Type::U32 | Type::U64, "<=") => format!("{result} = icmp ule {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::I8 | Type::I16 | Type::I32 | Type::I64 | Type::Enum(_), ">") => format!("{result} = icmp sgt {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::U8 | Type::U16 | Type::U32 | Type::U64, ">") => format!("{result} = icmp ugt {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::I8 | Type::I16 | Type::I32 | Type::I64 | Type::Enum(_), ">=") => format!("{result} = icmp sge {} {lhs}, {rhs}", lhs_ty.llvm()),
                    (Type::U8 | Type::U16 | Type::U32 | Type::U64, ">=") => format!("{result} = icmp uge {} {lhs}, {rhs}", lhs_ty.llvm()),
                    _ => return Err(format!("operator '{op}' is not valid for {}", lhs_ty.llvm())),
                };
                self.emit(instruction);
                Ok((result, result_ty))
            }
            Expr::Call { name, args } => {
                if name == "__ramlang_await" {
                    if args.len() != 1 {
                        return Err("await expects exactly one Future<T> expression".to_owned());
                    }
                    let (future, future_type) = self.emit_expr(&args[0])?;
                    let Type::Future(output) = future_type else {
                        return Err(format!(
                            "await expects Future<T>, found {}",
                            future_type.llvm()
                        ));
                    };
                    let encoded = self.fresh_value();
                    self.emit(format!(
                        "{encoded} = call i64 @ramlang_future_await_i64(ptr {future})"
                    ));
                    self.emit(format!("call void @ramlang_future_close(ptr {future})"));
                    let output_type = output.ty();
                    if output_type == Type::Void {
                        return Ok((String::new(), Type::Void));
                    }
                    let decoded = self.fresh_value();
                    match output_type {
                        Type::I8 => self.emit(format!("{decoded} = trunc i64 {encoded} to i8")),
                        Type::I16 => self.emit(format!("{decoded} = trunc i64 {encoded} to i16")),
                        Type::I32 | Type::Enum(_) => self.emit(format!("{decoded} = trunc i64 {encoded} to i32")),
                        Type::I64 => return Ok((encoded, Type::I64)),
                        Type::U8 => self.emit(format!("{decoded} = trunc i64 {encoded} to i8")),
                        Type::U16 => self.emit(format!("{decoded} = trunc i64 {encoded} to i16")),
                        Type::U32 => self.emit(format!("{decoded} = trunc i64 {encoded} to i32")),
                        Type::U64 => return Ok((encoded, Type::U64)),
                        Type::Bool => self.emit(format!("{decoded} = trunc i64 {encoded} to i1")),
                        Type::F32 => {
                            let bits = self.fresh_value();
                            self.emit(format!("{bits} = trunc i64 {encoded} to i32"));
                            self.emit(format!("{decoded} = bitcast i32 {bits} to float"));
                        }
                        Type::F64 => self.emit(format!("{decoded} = bitcast i64 {encoded} to double")),
                        _ => return Err("unsupported Future<T> output type in await".to_owned()),
                    }
                    return Ok((decoded, output_type));
                }

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
                        Type::I32 | Type::Enum(_) => self.emit(format!("call i32 (ptr, ...) @printf(ptr @.fmt.i32, i32 {value})")),
                        Type::I64 => self.emit(format!("call i32 (ptr, ...) @printf(ptr @.fmt.i64, i64 {value})")),
                        Type::U32 => self.emit(format!("call i32 (ptr, ...) @printf(ptr @.fmt.u32, i32 {value})")),
                        Type::U64 => self.emit(format!("call i32 (ptr, ...) @printf(ptr @.fmt.u64, i64 {value})")),
                        Type::F32 => {
                            let promoted = self.fresh_value();
                            self.emit(format!("{promoted} = fpext float {value} to double"));
                            self.emit(format!("call i32 (ptr, ...) @printf(ptr @.fmt.f64, double {promoted})"));
                        }
                        Type::F64 => self.emit(format!("call i32 (ptr, ...) @printf(ptr @.fmt.f64, double {value})")),
                        Type::Bool => {
                            let promoted = self.fresh_value();
                            self.emit(format!("{promoted} = zext i1 {value} to i32"));
                            self.emit(format!("call i32 (ptr, ...) @printf(ptr @.fmt.i32, i32 {promoted})"));
                        }
                        Type::Str => self.emit(format!("call i32 (ptr, ...) @printf(ptr @.fmt.str, ptr {value})")),
                        Type::Ptr => return Err("print does not accept raw pointers".to_owned()),
                        Type::Future(_) => return Err("print does not accept Future values directly".to_owned()),
                        Type::Array(_, _) => return Err("print does not accept arrays directly".to_owned()),
                        Type::Struct(_) => return Err("print does not accept structs directly".to_owned()),
                        Type::Void => return Err("print cannot print a void expression".to_owned()),
                    }
                    return Ok((String::new(), Type::Void));
                }

                if name == "assert" {
                    if args.len() != 1 {
                        return Err("assert expects exactly one bool argument".to_owned());
                    }
                    let (condition, condition_type) = self.emit_expr_expected(&args[0], Some(Type::Bool))?;
                    self.require_same(Type::Bool, condition_type, "assert condition")?;
                    let ok_label = self.fresh_label("assert.ok");
                    let trap_label = self.fresh_label("assert.trap");
                    self.emit(format!("br i1 {condition}, label %{ok_label}, label %{trap_label}"));
                    self.terminated = true;
                    self.label(&trap_label);
                    self.emit("call void @llvm.trap()");
                    self.emit("unreachable");
                    self.terminated = true;
                    self.label(&ok_label);
                    return Ok((String::new(), Type::Void));
                }

                if name == "assume" {
                    if args.len() != 1 {
                        return Err("assume expects exactly one bool argument".to_owned());
                    }
                    let (condition, condition_type) = self.emit_expr_expected(&args[0], Some(Type::Bool))?;
                    self.require_same(Type::Bool, condition_type, "assume condition")?;
                    self.emit(format!("call void @llvm.assume(i1 {condition})"));
                    return Ok((String::new(), Type::Void));
                }

                if name == "panic" {
                    if args.len() != 1 {
                        return Err("panic expects exactly one str argument".to_owned());
                    }
                    let (message, message_type) = self.emit_expr_expected(&args[0], Some(Type::Str))?;
                    self.require_same(Type::Str, message_type, "panic message")?;
                    self.emit(format!("call i32 (ptr, ...) @printf(ptr @.fmt.str, ptr {message})"));
                    self.emit("call void @llvm.trap()");
                    self.emit("unreachable");
                    self.terminated = true;
                    return Ok((String::new(), Type::Void));
                }

                if name == "strlen" {
                    if args.len() != 1 {
                        return Err("strlen expects exactly one string or pointer argument".to_owned());
                    }
                    let pointer = self.emit_raw_pointer_expr(&args[0], "strlen argument")?;
                    let size_t_type = self.size_t_llvm_type();
                    let raw_result = self.fresh_value();
                    self.emit(format!("{raw_result} = call {size_t_type} @strlen(ptr {pointer})"));
                    let result = self.widen_size_t_to_i64(&raw_result);
                    return Ok((result, Type::I64));
                }

                if matches!(name.as_str(), "strcmp" | "strncmp") {
                    let expected_args = if name == "strcmp" { 2 } else { 3 };
                    if args.len() != expected_args {
                        return Err(format!("{name} expects {expected_args} arguments"));
                    }
                    let left = self.emit_raw_pointer_expr(&args[0], &format!("{name} left operand"))?;
                    let right = self.emit_raw_pointer_expr(&args[1], &format!("{name} right operand"))?;
                    let result = self.fresh_value();
                    if name == "strcmp" {
                        self.emit(format!("{result} = call i32 @strcmp(ptr {left}, ptr {right})"));
                    } else {
                        let (count, count_type) = self.emit_expr_expected(&args[2], Some(Type::I64))?;
                        self.require_same(Type::I64, count_type, "strncmp byte count")?;
                        let count = self.emit_size_t_count(&count, "strncmp byte count")?;
                        let size_t_type = self.size_t_llvm_type();
                        self.emit(format!("{result} = call i32 @strncmp(ptr {left}, ptr {right}, {size_t_type} {count})"));
                    }
                    return Ok((result, Type::I32));
                }

                if name == "memchr" {
                    if args.len() != 3 {
                        return Err("memchr expects a pointer, byte value, and byte count".to_owned());
                    }
                    let pointer = self.emit_raw_pointer_expr(&args[0], "memchr buffer")?;
                    let (byte, byte_type) = self.emit_expr_expected(&args[1], Some(Type::I32))?;
                    self.require_same(Type::I32, byte_type, "memchr byte value")?;
                    let (count, count_type) = self.emit_expr_expected(&args[2], Some(Type::I64))?;
                    self.require_same(Type::I64, count_type, "memchr byte count")?;
                    let count = self.emit_size_t_count(&count, "memchr byte count")?;
                    let size_t_type = self.size_t_llvm_type();
                    let result = self.fresh_value();
                    self.emit(format!("{result} = call ptr @memchr(ptr {pointer}, i32 {byte}, {size_t_type} {count})"));
                    return Ok((result, Type::Ptr));
                }

                if matches!(name.as_str(), "memcpy" | "memmove") {
                    if args.len() != 3 {
                        return Err(format!("{name} expects destination, source, and byte count"));
                    }
                    let destination = self.emit_raw_pointer_expr(&args[0], "memory destination")?;
                    let source = self.emit_raw_pointer_expr(&args[1], "memory source")?;
                    let (count, count_type) = self.emit_expr_expected(&args[2], Some(Type::I64))?;
                    self.require_same(Type::I64, count_type, &format!("{name} byte count"))?;
                    let count = self.emit_size_t_count(&count, &format!("{name} byte count"))?;
                    let size_t_type = self.size_t_llvm_type();
                    let result = self.fresh_value();
                    self.emit(format!("{result} = call ptr @{name}(ptr {destination}, ptr {source}, {size_t_type} {count})"));
                    return Ok((result, Type::Ptr));
                }

                if name == "memset" {
                    if args.len() != 3 {
                        return Err("memset expects destination, fill value, and byte count".to_owned());
                    }
                    let destination = self.emit_raw_pointer_expr(&args[0], "memset destination")?;
                    let (fill, fill_type) = self.emit_expr_expected(&args[1], Some(Type::I32))?;
                    self.require_same(Type::I32, fill_type, "memset fill value")?;
                    let (count, count_type) = self.emit_expr_expected(&args[2], Some(Type::I64))?;
                    self.require_same(Type::I64, count_type, "memset byte count")?;
                    let count = self.emit_size_t_count(&count, "memset byte count")?;
                    let size_t_type = self.size_t_llvm_type();
                    let result = self.fresh_value();
                    self.emit(format!("{result} = call ptr @memset(ptr {destination}, i32 {fill}, {size_t_type} {count})"));
                    return Ok((result, Type::Ptr));
                }

                if name == "memcmp" {
                    if args.len() != 3 {
                        return Err("memcmp expects left pointer, right pointer, and byte count".to_owned());
                    }
                    let left = self.emit_raw_pointer_expr(&args[0], "memcmp left operand")?;
                    let right = self.emit_raw_pointer_expr(&args[1], "memcmp right operand")?;
                    let (count, count_type) = self.emit_expr_expected(&args[2], Some(Type::I64))?;
                    self.require_same(Type::I64, count_type, "memcmp byte count")?;
                    let count = self.emit_size_t_count(&count, "memcmp byte count")?;
                    let size_t_type = self.size_t_llvm_type();
                    let result = self.fresh_value();
                    self.emit(format!("{result} = call i32 @memcmp(ptr {left}, ptr {right}, {size_t_type} {count})"));
                    return Ok((result, Type::I32));
                }

                let overflow_ops = [
                    ("sadd_overflow", "sadd", false),
                    ("ssub_overflow", "ssub", false),
                    ("smul_overflow", "smul", false),
                    ("uadd_overflow", "uadd", true),
                    ("usub_overflow", "usub", true),
                    ("umul_overflow", "umul", true),
                ];
                let overflow_builtin = overflow_ops.iter().find_map(|(public_name, llvm_name, unsigned)| {
                    let type_name = name.strip_prefix(&format!("{public_name}_"))?;
                    let ty = match type_name {
                        "i8" => Type::I8,
                        "i16" => Type::I16,
                        "i32" => Type::I32,
                        "i64" => Type::I64,
                        "u8" => Type::U8,
                        "u16" => Type::U16,
                        "u32" => Type::U32,
                        "u64" => Type::U64,
                        _ => return None,
                    };
                    if ty.is_unsigned() != *unsigned {
                        return None;
                    }
                    Some((*llvm_name, ty))
                });
                if let Some((llvm_name, ty)) = overflow_builtin {
                    if args.len() != 3 {
                        return Err(format!("{name} expects left value, right value, and an output ptr"));
                    }
                    let (left, left_type) = self.emit_expr_expected(&args[0], Some(ty))?;
                    self.require_same(ty, left_type, &format!("{name} left operand"))?;
                    let (right, right_type) = self.emit_expr_expected(&args[1], Some(ty))?;
                    self.require_same(ty, right_type, &format!("{name} right operand"))?;
                    let (output, output_type) = self.emit_expr_expected(&args[2], Some(Type::Ptr))?;
                    self.require_same(Type::Ptr, output_type, &format!("{name} output pointer"))?;

                    let llvm_type = ty.llvm();
                    let aggregate_type = format!("{{ {llvm_type}, i1 }}");
                    let aggregate = self.fresh_value();
                    self.emit(format!(
                        "{aggregate} = call {aggregate_type} @llvm.{llvm_name}.with.overflow.{llvm_type}({llvm_type} {left}, {llvm_type} {right})"
                    ));
                    let result_value = self.fresh_value();
                    self.emit(format!("{result_value} = extractvalue {aggregate_type} {aggregate}, 0"));
                    let overflow = self.fresh_value();
                    self.emit(format!("{overflow} = extractvalue {aggregate_type} {aggregate}, 1"));
                    self.emit(format!("store {llvm_type} {result_value}, ptr {output}"));
                    return Ok((overflow, Type::Bool));
                }

                let saturating_ops = [
                    ("sadd_sat", "sadd.sat", false),
                    ("ssub_sat", "ssub.sat", false),
                    ("uadd_sat", "uadd.sat", true),
                    ("usub_sat", "usub.sat", true),
                ];
                let saturating_builtin = saturating_ops.iter().find_map(|(public_name, llvm_name, unsigned)| {
                    let type_name = name.strip_prefix(&format!("{public_name}_"))?;
                    let ty = match type_name {
                        "i8" => Type::I8,
                        "i16" => Type::I16,
                        "i32" => Type::I32,
                        "i64" => Type::I64,
                        "u8" => Type::U8,
                        "u16" => Type::U16,
                        "u32" => Type::U32,
                        "u64" => Type::U64,
                        _ => return None,
                    };
                    if ty.is_unsigned() != *unsigned {
                        return None;
                    }
                    Some((*llvm_name, ty))
                });
                if let Some((llvm_name, ty)) = saturating_builtin {
                    if args.len() != 2 {
                        return Err(format!("{name} expects exactly two integer arguments"));
                    }
                    let (left, left_type) = self.emit_expr_expected(&args[0], Some(ty))?;
                    self.require_same(ty, left_type, &format!("{name} left operand"))?;
                    let (right, right_type) = self.emit_expr_expected(&args[1], Some(ty))?;
                    self.require_same(ty, right_type, &format!("{name} right operand"))?;
                    let llvm_type = ty.llvm();
                    let result = self.fresh_value();
                    self.emit(format!(
                        "{result} = call {llvm_type} @llvm.{llvm_name}.{llvm_type}({llvm_type} {left}, {llvm_type} {right})"
                    ));
                    return Ok((result, ty));
                }

                let bit_intrinsic = ["ctpop", "ctlz", "cttz", "bswap", "bitreverse"]
                    .iter()
                    .find_map(|intrinsic| {
                        let suffix = name.strip_prefix(&format!("{intrinsic}_"))?;
                        let width = match suffix {
                            "8" => 8,
                            "16" => 16,
                            "32" => 32,
                            "64" => 64,
                            _ => return None,
                        };
                        if *intrinsic == "bswap" && width == 8 {
                            return None;
                        }
                        Some((*intrinsic, width))
                    });
                if let Some((intrinsic, width)) = bit_intrinsic {
                    if args.len() != 1 {
                        return Err(format!("{name} expects exactly one integer argument"));
                    }
                    let (value, value_type) = self.emit_expr(&args[0])?;
                    if value_type.integer_width() != Some(width) {
                        return Err(format!("{name} expects an integer with width {width}, found {}", value_type.llvm()));
                    }
                    let llvm_type = value_type.llvm();
                    let result = self.fresh_value();
                    match intrinsic {
                        "ctlz" | "cttz" => {
                            self.emit(format!(
                                "{result} = call {llvm_type} @llvm.{intrinsic}.{llvm_type}({llvm_type} {value}, i1 false)"
                            ));
                        }
                        _ => {
                            self.emit(format!(
                                "{result} = call {llvm_type} @llvm.{intrinsic}.{llvm_type}({llvm_type} {value})"
                            ));
                        }
                    }
                    return Ok((result, value_type));
                }

                let mut atomic_builtin: Option<(&str, Type, &str)> = None;
                let atomic_types = [
                    ("i8", Type::I8), ("i16", Type::I16), ("i32", Type::I32), ("i64", Type::I64),
                    ("u8", Type::U8), ("u16", Type::U16), ("u32", Type::U32), ("u64", Type::U64),
                ];
                let atomic_ops = [
                    ("load", "load"), ("store", "store"), ("exchange", "xchg"),
                    ("fetch_add", "add"), ("fetch_sub", "sub"), ("fetch_and", "and"),
                    ("fetch_or", "or"), ("fetch_xor", "xor"), ("fetch_nand", "nand"),
                    ("fetch_max", "max"), ("fetch_min", "min"),
                    ("fetch_umax", "umax"), ("fetch_umin", "umin"),
                    ("compare_exchange", "cmpxchg"),
                ];
                'atomic_parse: for (operation, _) in atomic_ops {
                    let Some(rest) = name.strip_prefix(&format!("atomic_{operation}_")) else {
                        continue;
                    };
                    for (type_name, atomic_type) in atomic_types {
                        if matches!(operation, "fetch_max" | "fetch_min") && !type_name.starts_with("i") {
                            continue;
                        }
                        if matches!(operation, "fetch_umax" | "fetch_umin") && !type_name.starts_with("u") {
                            continue;
                        }
                        if rest == type_name {
                            atomic_builtin = Some((operation, atomic_type, "seq_cst"));
                            break 'atomic_parse;
                        }
                        let Some(order_suffix) = rest.strip_prefix(&format!("{type_name}_")) else {
                            continue;
                        };
                        let order = match order_suffix {
                            "relaxed" => Some("monotonic"),
                            "acquire" => Some("acquire"),
                            "release" => Some("release"),
                            "acq_rel" => Some("acq_rel"),
                            "seq_cst" => Some("seq_cst"),
                            _ => None,
                        };
                        if let Some(order) = order {
                            atomic_builtin = Some((operation, atomic_type, order));
                            break 'atomic_parse;
                        }
                    }
                }
                if let Some((operation, atomic_type, order)) = atomic_builtin {
                    let expected_args = match operation {
                        "load" => 1,
                        "compare_exchange" => 3,
                        _ => 2,
                    };
                    if args.len() != expected_args {
                        return Err(format!("atomic_{operation}_* expects {expected_args} argument(s)"));
                    }
                    if operation == "load" && matches!(order, "release" | "acq_rel") {
                        return Err("atomic loads support relaxed, acquire, or seq_cst ordering".to_owned());
                    }
                    if operation == "store" && matches!(order, "acquire" | "acq_rel") {
                        return Err("atomic stores support relaxed, release, or seq_cst ordering".to_owned());
                    }
                    let pointer = self.emit_raw_pointer_expr(&args[0], "atomic address")?;
                    let llvm_type = atomic_type.llvm();
                    let alignment = atomic_type.integer_width().unwrap() / 8;
                    if operation == "load" {
                        let result = self.fresh_value();
                        self.emit(format!(
                            "{result} = load atomic {llvm_type}, ptr {pointer} {order}, align {alignment}"
                        ));
                        return Ok((result, atomic_type));
                    }
                    if operation == "compare_exchange" {
                        let expected_pointer = self.emit_raw_pointer_expr(&args[1], "atomic expected-value address")?;
                        let expected_value = self.fresh_value();
                        self.emit(format!(
                            "{expected_value} = load {llvm_type}, ptr {expected_pointer}, align {alignment}"
                        ));
                        let (desired, desired_type) = self.emit_expr_expected(&args[2], Some(atomic_type))?;
                        self.require_same(atomic_type, desired_type, "atomic compare-exchange desired value")?;
                        let failure_order = match order {
                            "release" => "monotonic",
                            "acq_rel" => "acquire",
                            other => other,
                        };
                        let pair_type = format!("{{ {llvm_type}, i1 }}");
                        let pair = self.fresh_value();
                        self.emit(format!(
                            "{pair} = cmpxchg ptr {pointer}, {llvm_type} {expected_value}, {llvm_type} {desired} {order} {failure_order}, align {alignment}"
                        ));
                        let observed = self.fresh_value();
                        self.emit(format!(
                            "{observed} = extractvalue {pair_type} {pair}, 0"
                        ));
                        let success = self.fresh_value();
                        self.emit(format!(
                            "{success} = extractvalue {pair_type} {pair}, 1"
                        ));
                        self.emit(format!(
                            "store {llvm_type} {observed}, ptr {expected_pointer}, align {alignment}"
                        ));
                        return Ok((success, Type::Bool));
                    }
                    let (value, value_type) = self.emit_expr_expected(&args[1], Some(atomic_type))?;
                    self.require_same(atomic_type, value_type, &format!("atomic_{operation} value"))?;
                    if operation == "store" {
                        self.emit(format!(
                            "store atomic {llvm_type} {value}, ptr {pointer} {order}, align {alignment}"
                        ));
                        return Ok((String::new(), Type::Void));
                    }
                    let llvm_operation = atomic_ops.iter()
                        .find(|(name, _)| *name == operation)
                        .map(|(_, llvm_name)| *llvm_name)
                        .unwrap();
                    let result = self.fresh_value();
                    self.emit(format!(
                        "{result} = atomicrmw {llvm_operation} ptr {pointer}, {llvm_type} {value} {order}"
                    ));
                    return Ok((result, atomic_type));
                }

                let fence_order = match name.as_str() {
                    "fence_acquire" => Some("acquire"),
                    "fence_release" => Some("release"),
                    "fence_acq_rel" => Some("acq_rel"),
                    "fence_seq_cst" => Some("seq_cst"),
                    _ => None,
                };
                if let Some(order) = fence_order {
                    if !args.is_empty() {
                        return Err(format!("{name} expects no arguments"));
                    }
                    self.emit(format!("fence {order}"));
                    return Ok((String::new(), Type::Void));
                }

                let inline_asm_type = match name.as_str() {
                    "asm_void" => Some(Type::Void),
                    "asm_i8" => Some(Type::I8),
                    "asm_i16" => Some(Type::I16),
                    "asm_i32" => Some(Type::I32),
                    "asm_i64" => Some(Type::I64),
                    "asm_u8" => Some(Type::U8),
                    "asm_u16" => Some(Type::U16),
                    "asm_u32" => Some(Type::U32),
                    "asm_u64" => Some(Type::U64),
                    "asm_f32" => Some(Type::F32),
                    "asm_f64" => Some(Type::F64),
                    "asm_bool" => Some(Type::Bool),
                    "asm_ptr" => Some(Type::Ptr),
                    _ => None,
                };
                if let Some(return_type) = inline_asm_type {
                    if args.len() < 2 {
                        return Err(format!("{name} expects an assembly string and a constraint string"));
                    }
                    let Expr::String(assembly) = &args[0] else {
                        return Err(format!("{name} assembly template must be a string literal"));
                    };
                    let Expr::String(constraints) = &args[1] else {
                        return Err(format!("{name} constraints must be a string literal"));
                    };
                    let encode_llvm_bytes = |value: &str| {
                        value.as_bytes().iter().map(|byte| format!("\\{:02X}", byte)).collect::<String>()
                    };
                    let mut operands = Vec::new();
                    for argument in args.iter().skip(2) {
                        let (value, value_type) = self.emit_expr(argument)?;
                        if value_type == Type::Void {
                            return Err(format!("{name} does not accept void input operands"));
                        }
                        operands.push(format!("{} {value}", value_type.llvm()));
                    }
                    let assembly = encode_llvm_bytes(assembly);
                    let constraints = encode_llvm_bytes(constraints);
                    let operands = operands.join(", ");
                    let asm_call = if return_type == Type::Void {
                        format!("call void asm sideeffect \"{assembly}\", \"{constraints}\"({operands})")
                    } else {
                        let result = self.fresh_value();
                        self.emit(format!("{result} = call {} asm sideeffect \"{assembly}\", \"{constraints}\"({operands})", return_type.llvm()));
                        return Ok((result, return_type));
                    };
                    self.emit(asm_call);
                    return Ok((String::new(), Type::Void));
                }

                if name == "alloc" {
                    if args.len() != 1 {
                        return Err("alloc expects one byte-count argument".to_owned());
                    }
                    let (size, size_type) = self.emit_expr_expected(&args[0], Some(Type::I64))?;
                    self.require_same(Type::I64, size_type, "alloc byte count")?;
                    let size = self.emit_size_t_count(&size, "alloc byte count")?;
                    let size_t_type = self.size_t_llvm_type();
                    let result = self.fresh_value();
                    self.emit(format!("{result} = call ptr @malloc({size_t_type} {size})"));
                    return Ok((result, Type::Ptr));
                }

                if name == "calloc" {
                    if args.len() != 2 {
                        return Err("calloc expects an element count and element size".to_owned());
                    }
                    let (count, count_type) = self.emit_expr_expected(&args[0], Some(Type::I64))?;
                    self.require_same(Type::I64, count_type, "calloc element count")?;
                    let count = self.emit_size_t_count(&count, "calloc element count")?;
                    let (size, size_type) = self.emit_expr_expected(&args[1], Some(Type::I64))?;
                    self.require_same(Type::I64, size_type, "calloc element size")?;
                    let size = self.emit_size_t_count(&size, "calloc element size")?;
                    let size_t_type = self.size_t_llvm_type();
                    let result = self.fresh_value();
                    self.emit(format!("{result} = call ptr @calloc({size_t_type} {count}, {size_t_type} {size})"));
                    return Ok((result, Type::Ptr));
                }

                if name == "realloc" {
                    if args.len() != 2 {
                        return Err("realloc expects an existing pointer and a new byte count".to_owned());
                    }
                    let pointer = self.emit_raw_pointer_expr(&args[0], "realloc pointer")?;
                    let (size, size_type) = self.emit_expr_expected(&args[1], Some(Type::I64))?;
                    self.require_same(Type::I64, size_type, "realloc byte count")?;
                    let size = self.emit_size_t_count(&size, "realloc byte count")?;
                    let size_t_type = self.size_t_llvm_type();
                    let result = self.fresh_value();
                    self.emit(format!("{result} = call ptr @realloc(ptr {pointer}, {size_t_type} {size})"));
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

                let volatile_load = name.starts_with("volatile_load_");
                let unaligned_load = name.contains("_unaligned_");
                let loaded_type = match name.as_str() {
                    "load_i8" | "volatile_load_i8" | "load_unaligned_i8" | "volatile_load_unaligned_i8" => Some(Type::I8),
                    "load_i16" | "volatile_load_i16" | "load_unaligned_i16" | "volatile_load_unaligned_i16" => Some(Type::I16),
                    "load_i32" | "volatile_load_i32" | "load_unaligned_i32" | "volatile_load_unaligned_i32" => Some(Type::I32),
                    "load_i64" | "volatile_load_i64" | "load_unaligned_i64" | "volatile_load_unaligned_i64" => Some(Type::I64),
                    "load_u8" | "volatile_load_u8" | "load_unaligned_u8" | "volatile_load_unaligned_u8" => Some(Type::U8),
                    "load_u16" | "volatile_load_u16" | "load_unaligned_u16" | "volatile_load_unaligned_u16" => Some(Type::U16),
                    "load_u32" | "volatile_load_u32" | "load_unaligned_u32" | "volatile_load_unaligned_u32" => Some(Type::U32),
                    "load_u64" | "volatile_load_u64" | "load_unaligned_u64" | "volatile_load_unaligned_u64" => Some(Type::U64),
                    "load_f32" | "volatile_load_f32" | "load_unaligned_f32" | "volatile_load_unaligned_f32" => Some(Type::F32),
                    "load_f64" | "volatile_load_f64" | "load_unaligned_f64" | "volatile_load_unaligned_f64" => Some(Type::F64),
                    "load_bool" | "volatile_load_bool" | "load_unaligned_bool" | "volatile_load_unaligned_bool" => Some(Type::Bool),
                    "load_ptr" | "volatile_load_ptr" | "load_unaligned_ptr" | "volatile_load_unaligned_ptr" => Some(Type::Ptr),
                    _ => None,
                };
                if let Some(loaded_type) = loaded_type {
                    if args.len() != 1 {
                        return Err(format!("{name} expects exactly one ptr argument"));
                    }
                    let (pointer, pointer_type) = self.emit_expr_expected(&args[0], Some(Type::Ptr))?;
                    self.require_same(Type::Ptr, pointer_type, &format!("{name} argument"))?;
                    let result = self.fresh_value();
                    if unaligned_load && volatile_load {
                        self.emit(format!("{result} = load volatile {}, ptr {pointer}, align 1", loaded_type.llvm()));
                    } else if unaligned_load {
                        self.emit(format!("{result} = load {}, ptr {pointer}, align 1", loaded_type.llvm()));
                    } else if volatile_load {
                        self.emit(format!("{result} = load volatile {}, ptr {pointer}", loaded_type.llvm()));
                    } else {
                        self.emit(format!("{result} = load {}, ptr {pointer}", loaded_type.llvm()));
                    }
                    return Ok((result, loaded_type));
                }

                let volatile_store = name.starts_with("volatile_store_");
                let unaligned_store = name.contains("_unaligned_");
                let stored_type = match name.as_str() {
                    "store_i8" | "volatile_store_i8" | "store_unaligned_i8" | "volatile_store_unaligned_i8" => Some(Type::I8),
                    "store_i16" | "volatile_store_i16" | "store_unaligned_i16" | "volatile_store_unaligned_i16" => Some(Type::I16),
                    "store_i32" | "volatile_store_i32" | "store_unaligned_i32" | "volatile_store_unaligned_i32" => Some(Type::I32),
                    "store_i64" | "volatile_store_i64" | "store_unaligned_i64" | "volatile_store_unaligned_i64" => Some(Type::I64),
                    "store_u8" | "volatile_store_u8" | "store_unaligned_u8" | "volatile_store_unaligned_u8" => Some(Type::U8),
                    "store_u16" | "volatile_store_u16" | "store_unaligned_u16" | "volatile_store_unaligned_u16" => Some(Type::U16),
                    "store_u32" | "volatile_store_u32" | "store_unaligned_u32" | "volatile_store_unaligned_u32" => Some(Type::U32),
                    "store_u64" | "volatile_store_u64" | "store_unaligned_u64" | "volatile_store_unaligned_u64" => Some(Type::U64),
                    "store_f32" | "volatile_store_f32" | "store_unaligned_f32" | "volatile_store_unaligned_f32" => Some(Type::F32),
                    "store_f64" | "volatile_store_f64" | "store_unaligned_f64" | "volatile_store_unaligned_f64" => Some(Type::F64),
                    "store_bool" | "volatile_store_bool" | "store_unaligned_bool" | "volatile_store_unaligned_bool" => Some(Type::Bool),
                    "store_ptr" | "volatile_store_ptr" | "store_unaligned_ptr" | "volatile_store_unaligned_ptr" => Some(Type::Ptr),
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
                    if unaligned_store && volatile_store {
                        self.emit(format!("store volatile {} {value}, ptr {pointer}, align 1", stored_type.llvm()));
                    } else if unaligned_store {
                        self.emit(format!("store {} {value}, ptr {pointer}, align 1", stored_type.llvm()));
                    } else if volatile_store {
                        self.emit(format!("store volatile {} {value}, ptr {pointer}", stored_type.llvm()));
                    } else {
                        self.emit(format!("store {} {value}, ptr {pointer}", stored_type.llvm()));
                    }
                    return Ok((String::new(), Type::Void));
                }

                let signature = self.signatures.get(name).cloned()
                    .ok_or_else(|| format!("unknown function '{name}'"))?;
                if (!signature.variadic && signature.params.len() != args.len())
                    || (signature.variadic && args.len() < signature.params.len())
                {
                    return Err(if signature.variadic {
                        format!("variadic function '{name}' expects at least {} argument(s), got {}", signature.params.len(), args.len())
                    } else {
                        format!("function '{name}' expects {} argument(s), got {}", signature.params.len(), args.len())
                    });
                }
                let mut lowered = Vec::new();
                let mut lowered_values = Vec::<(Type, String)>::new();
                for (arg, expected) in args.iter().take(signature.params.len()).zip(signature.params.iter()) {
                    let (value, actual) = self.emit_expr_expected(arg, Some(*expected))?;
                    self.require_same(*expected, actual, &format!("argument to '{name}'"))?;
                    lowered.push(format!("{} {value}", actual.llvm()));
                    lowered_values.push((actual, value));
                }
                if signature.variadic {
                    for arg in args.iter().skip(signature.params.len()) {
                        let (mut value, mut actual) = self.emit_expr(arg)?;
                        match actual {
                            Type::Bool => {
                                let promoted = self.fresh_value();
                                self.emit(format!("{promoted} = zext i1 {value} to i32"));
                                value = promoted;
                                actual = Type::I32;
                            }
                            Type::I8 => {
                                let promoted = self.fresh_value();
                                self.emit(format!("{promoted} = sext i8 {value} to i32"));
                                value = promoted;
                                actual = Type::I32;
                            }
                            Type::U8 => {
                                let promoted = self.fresh_value();
                                self.emit(format!("{promoted} = zext i8 {value} to i32"));
                                value = promoted;
                                actual = Type::I32;
                            }
                            Type::I16 => {
                                let promoted = self.fresh_value();
                                self.emit(format!("{promoted} = sext i16 {value} to i32"));
                                value = promoted;
                                actual = Type::I32;
                            }
                            Type::U16 => {
                                let promoted = self.fresh_value();
                                self.emit(format!("{promoted} = zext i16 {value} to i32"));
                                value = promoted;
                                actual = Type::I32;
                            }
                            Type::F32 => {
                                let promoted = self.fresh_value();
                                self.emit(format!("{promoted} = fpext float {value} to double"));
                                value = promoted;
                                actual = Type::F64;
                            }
                            Type::Array(_, _) | Type::Struct(_) => {
                                return Err(format!("variadic argument to '{name}' cannot pass arrays or structs by value"));
                            }
                            Type::Void => return Err(format!("variadic argument to '{name}' cannot have type void")),
                            _ => {}
                        }
                        lowered.push(format!("{} {value}", actual.llvm()));
                    }
                }
                if signature.asynchronous {
                    let output = FutureOutput::from_type(signature.return_type).ok_or_else(|| {
                        format!("async function '{name}' has an unsupported Future output type")
                    })?;
                    let frame_type = async_frame_type_name(name);
                    let size_pointer = self.fresh_value();
                    self.emit(format!(
                        "{size_pointer} = getelementptr {frame_type}, ptr null, i64 1"
                    ));
                    let frame_size = self.fresh_value();
                    self.emit(format!("{frame_size} = ptrtoint ptr {size_pointer} to i64"));
                    let frame = self.fresh_value();
                    self.emit(format!("{frame} = call ptr @ramlang_alloc(i64 {frame_size})"));
                    for (index, (argument_type, value)) in lowered_values.iter().enumerate() {
                        let field = self.fresh_value();
                        self.emit(format!(
                            "{field} = getelementptr inbounds {frame_type}, ptr {frame}, i32 0, i32 {index}"
                        ));
                        self.emit(format!(
                            "store {} {value}, ptr {field}",
                            argument_type.llvm()
                        ));
                    }
                    let future = self.fresh_value();
                    let entry = async_entry_symbol(name);
                    self.emit(format!(
                        "{future} = call ptr @ramlang_future_spawn_i64(ptr @{entry}, ptr {frame})"
                    ));
                    return Ok((future, Type::Future(output)));
                }

                let arguments = lowered.join(", ");
                if signature.return_type == Type::Void {
                    if signature.variadic {
                        let fixed = signature.params.iter().map(|ty| ty.llvm()).collect::<Vec<_>>().join(", ");
                        self.emit(format!("call void ({fixed}, ...) @{name}({arguments})"));
                    } else {
                        self.emit(format!("call void @{name}({arguments})"));
                    }
                    Ok((String::new(), Type::Void))
                } else {
                    let result = self.fresh_value();
                    if signature.variadic {
                        let fixed = signature.params.iter().map(|ty| ty.llvm()).collect::<Vec<_>>().join(", ");
                        self.emit(format!("{result} = call {} ({fixed}, ...) @{name}({arguments})", signature.return_type.llvm()));
                    } else {
                        self.emit(format!("{result} = call {} @{name}({arguments})", signature.return_type.llvm()));
                    }
                    Ok((result, signature.return_type))
                }
            }
        }
    }

    fn enum_variant_identity(&self, expression: &Expr) -> Option<(u64, i32)> {
        let Expr::Field { base, field } = expression else {
            return None;
        };
        let Expr::Variable(enum_name) = base.as_ref() else {
            return None;
        };
        let type_id = struct_type_id(enum_name);
        let enumeration = self.enums.get(&type_id)?;
        let (_, value) = enumeration.variants.iter().find(|(variant, _)| variant == field)?;
        Some((type_id, *value))
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
                self.scopes.last_mut().unwrap().insert(name.clone(), Binding {
                    ty, slot, mutable: *mutable, read_only_pointee: false,
                });
            }
            Stmt::FieldAssign { base, field, value } => {
                if let Expr::Variable(base_name) = base {
                    let binding = self.lookup(base_name)
                        .ok_or_else(|| format!("field assignment on unknown variable '{base_name}'"))?;
                    if !binding.mutable {
                        return Err(format!("cannot assign to field '{field}' of immutable variable '{base_name}'; declare it with 'let mut'"));
                    }
                }
                let (field_ptr, field_type, packed) = self.emit_struct_field_ptr(base, field)?;
                let (stored_value, actual_type) = self.emit_expr_expected(value, Some(field_type))?;
                self.require_same(field_type, actual_type, &format!("assignment to field '{field}'"))?;
                if packed {
                    self.emit(format!("store {} {stored_value}, ptr {field_ptr}, align 1", field_type.llvm()));
                } else {
                    self.emit(format!("store {} {stored_value}, ptr {field_ptr}", field_type.llvm()));
                }
            }
            Stmt::IndexAssign { array, index, value } => {
                if let Expr::Variable(name) = array {
                    if self.lookup(name).is_some_and(|binding| binding.read_only_pointee) {
                        return Err(format!("cannot assign to an element of immutable static array '{name}'"));
                    }
                }
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
            Stmt::Match { value, arms, default } => {
                let (scrutinee, scrutinee_type) = self.emit_expr(value)?;
                if !scrutinee_type.is_integer() && scrutinee_type != Type::Bool {
                    return Err("match currently requires an integer, bool, or enum value".to_owned());
                }

                if default.is_none() {
                    if scrutinee_type == Type::Bool {
                        let mut covered = HashSet::<bool>::new();
                        for (pattern, _) in arms {
                            let Expr::Bool(value) = pattern else {
                                return Err("exhaustive bool match requires true and false literal patterns or a '_' fallback".to_owned());
                            };
                            if !covered.insert(*value) {
                                return Err(format!("duplicate boolean pattern '{value}' in match"));
                            }
                        }
                        if covered.len() != 2 {
                            let missing = if !covered.contains(&true) { "true" } else { "false" };
                            return Err(format!("non-exhaustive match on bool; missing pattern: {missing}"));
                        }
                    } else {
                        let Type::Enum(enum_id) = scrutinee_type else {
                            return Err("match on non-enum values requires a '_' fallback arm".to_owned());
                        };
                        let enumeration = self.enums.get(&enum_id)
                            .ok_or_else(|| format!("unknown enum type id {enum_id:016x}"))?;
                        let required = enumeration.variants.iter().map(|(_, value)| *value).collect::<HashSet<_>>();
                        let mut covered = HashSet::<i32>::new();
                        for (pattern, _) in arms {
                            let Some((pattern_enum_id, discriminant)) = self.enum_variant_identity(pattern) else {
                                return Err(format!("match on enum '{}' requires named enum variants or a '_' fallback", enumeration.name));
                            };
                            if pattern_enum_id != enum_id {
                                return Err(format!("match arm belongs to a different enum than '{}'", enumeration.name));
                            }
                            if !covered.insert(discriminant) {
                                return Err(format!("duplicate discriminant {discriminant} in match on enum '{}'", enumeration.name));
                            }
                        }
                        if covered != required {
                            let missing = enumeration.variants.iter()
                                .filter(|(_, value)| !covered.contains(value))
                                .map(|(name, _)| name.as_str())
                                .collect::<Vec<_>>();
                            return Err(format!(
                                "non-exhaustive match on enum '{}'; missing variant(s): {}",
                                enumeration.name,
                                missing.join(", ")
                            ));
                        }
                    }
                }

                // Use an LLVM switch for compile-time constant patterns. If any
                // pattern is dynamic, retain the expression-comparison chain below.
                let mut switch_values = Vec::<String>::new();
                let mut switch_possible = true;
                let mut unique_switch_values = HashSet::<String>::new();
                for (pattern, _) in arms {
                    let case_value = if let Type::Enum(enum_id) = scrutinee_type {
                        if let Some((pattern_enum_id, discriminant)) = self.enum_variant_identity(pattern) {
                            if pattern_enum_id != enum_id {
                                switch_possible = false;
                                break;
                            }
                            Some(discriminant.to_string())
                        } else {
                            None
                        }
                    } else if scrutinee_type == Type::Bool {
                        match pattern {
                            Expr::Bool(value) => Some(if *value { "true" } else { "false" }.to_owned()),
                            _ => None,
                        }
                    } else if scrutinee_type.is_integer() {
                        constant_integer_value(pattern)?.map(|value| value.to_string())
                    } else {
                        None
                    };

                    let Some(case_value) = case_value else {
                        switch_possible = false;
                        break;
                    };
                    let (_, pattern_type) = self.emit_expr_expected(pattern, Some(scrutinee_type))?;
                    self.require_same(scrutinee_type, pattern_type, "match arm pattern")?;
                    if !unique_switch_values.insert(case_value.clone()) {
                        switch_possible = false;
                        break;
                    }
                    switch_values.push(case_value);
                }

                if switch_possible {
                    let end_label = self.fresh_label("match.end");
                    let arm_labels = arms.iter().map(|_| self.fresh_label("match.arm")).collect::<Vec<_>>();
                    let fallback_label = self.fresh_label(if default.is_some() { "match.default" } else { "match.invalid" });
                    let mut switch = format!(
                        "switch {} {scrutinee}, label %{fallback_label} [",
                        scrutinee_type.llvm()
                    );
                    for (case_value, arm_label) in switch_values.iter().zip(&arm_labels) {
                        switch.push_str(&format!(
                            "\n    {} {case_value}, label %{arm_label}",
                            scrutinee_type.llvm()
                        ));
                    }
                    switch.push_str("\n  ]");
                    self.emit(switch);
                    self.terminated = true;

                    let mut has_fallthrough = false;
                    for ((_, body), arm_label) in arms.iter().zip(&arm_labels) {
                        self.label(arm_label);
                        self.emit_stmt(body)?;
                        if !self.terminated {
                            self.emit(format!("br label %{end_label}"));
                            self.terminated = true;
                            has_fallthrough = true;
                        }
                    }

                    self.label(&fallback_label);
                    if let Some(default) = default {
                        self.emit_stmt(default)?;
                        if !self.terminated {
                            self.emit(format!("br label %{end_label}"));
                            self.terminated = true;
                            has_fallthrough = true;
                        }
                    } else {
                        self.emit("call void @llvm.trap()");
                        self.emit("unreachable");
                        self.terminated = true;
                    }

                    if has_fallthrough {
                        self.label(&end_label);
                    } else {
                        self.terminated = true;
                    }
                    return Ok(());
                }

                let end_label = self.fresh_label("match.end");
                let mut has_fallthrough = false;
                for (pattern, body) in arms {
                    let (pattern_value, pattern_type) =
                        self.emit_expr_expected(pattern, Some(scrutinee_type))?;
                    self.require_same(scrutinee_type, pattern_type, "match arm pattern")?;
                    let equal = self.fresh_value();
                    self.emit(format!("{equal} = icmp eq {} {scrutinee}, {pattern_value}", scrutinee_type.llvm()));
                    let arm_label = self.fresh_label("match.arm");
                    let next_label = self.fresh_label("match.next");
                    self.emit(format!("br i1 {equal}, label %{arm_label}, label %{next_label}"));
                    self.terminated = true;

                    self.label(&arm_label);
                    self.emit_stmt(body)?;
                    if !self.terminated {
                        self.emit(format!("br label %{end_label}"));
                        self.terminated = true;
                        has_fallthrough = true;
                    }
                    self.label(&next_label);
                }

                if let Some(default) = default {
                    self.emit_stmt(default)?;
                    if !self.terminated {
                        self.emit(format!("br label %{end_label}"));
                        self.terminated = true;
                        has_fallthrough = true;
                    }
                } else {
                    self.emit("call void @llvm.trap()");
                    self.emit("unreachable");
                    self.terminated = true;
                }

                if has_fallthrough {
                    self.label(&end_label);
                } else {
                    self.terminated = true;
                }
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
            self.scopes[0].insert(name.clone(), Binding {
                ty: *ty, slot, mutable: false, read_only_pointee: false,
            });
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

fn async_frame_type_name(name: &str) -> String {
    format!("%__ramlang_async_args_{:016x}", struct_type_id(name))
}

fn async_entry_symbol(name: &str) -> String {
    format!("__ramlang_async_entry_{:016x}", struct_type_id(name))
}

fn emit_async_entry_wrapper(function: &Function) -> String {
    let frame_type = async_frame_type_name(&function.name);
    let entry = async_entry_symbol(&function.name);
    let mut output = format!("define i64 @{entry}(ptr %context) {{\nentry:\n");
    let mut args = Vec::new();
    for (index, (_, ty)) in function.params.iter().enumerate() {
        let field_ptr = format!("%field_ptr{index}");
        let value = format!("%arg{index}");
        output.push_str(&format!(
            "  {field_ptr} = getelementptr inbounds {frame_type}, ptr %context, i32 0, i32 {index}\n"
        ));
        output.push_str(&format!("  {value} = load {}, ptr {field_ptr}\n", ty.llvm()));
        args.push(format!("{} {value}", ty.llvm()));
    }
    let arguments = args.join(", ");
    if function.return_type == Type::Void {
        output.push_str(&format!("  call void @{}({arguments})\n", function.name));
        output.push_str("  call void @ramlang_free(ptr %context)\n  ret i64 0\n}\n");
        return output;
    }

    output.push_str(&format!(
        "  %result = call {} @{}({arguments})\n",
        function.return_type.llvm(),
        function.name
    ));
    output.push_str("  call void @ramlang_free(ptr %context)\n");
    match function.return_type {
        Type::I8 => output.push_str("  %encoded = sext i8 %result to i64\n  ret i64 %encoded\n"),
        Type::I16 => output.push_str("  %encoded = sext i16 %result to i64\n  ret i64 %encoded\n"),
        Type::I32 | Type::Enum(_) => output.push_str("  %encoded = sext i32 %result to i64\n  ret i64 %encoded\n"),
        Type::I64 => output.push_str("  ret i64 %result\n"),
        Type::U8 => output.push_str("  %encoded = zext i8 %result to i64\n  ret i64 %encoded\n"),
        Type::Bool => output.push_str("  %encoded = zext i1 %result to i64\n  ret i64 %encoded\n"),
        Type::U16 => output.push_str("  %encoded = zext i16 %result to i64\n  ret i64 %encoded\n"),
        Type::U32 => output.push_str("  %encoded = zext i32 %result to i64\n  ret i64 %encoded\n"),
        Type::U64 => output.push_str("  ret i64 %result\n"),
        Type::F32 => {
            output.push_str("  %result_bits = bitcast float %result to i32\n");
            output.push_str("  %encoded = zext i32 %result_bits to i64\n  ret i64 %encoded\n");
        }
        Type::F64 => output.push_str("  %encoded = bitcast double %result to i64\n  ret i64 %encoded\n"),
        _ => output.push_str("  ret i64 0\n"),
    }
    output.push_str("}\n");
    output
}

fn statement_always_returns(statement: &Stmt) -> bool {
    match statement {
        Stmt::Return(_) => true,
        Stmt::Expression(Expr::Call { name, .. }) if name == "panic" => true,
        Stmt::Block(statements) => statements.iter().any(statement_always_returns),
        Stmt::If { then_branch, else_branch: Some(else_branch), .. } => {
            statement_always_returns(then_branch) && statement_always_returns(else_branch)
        }
        Stmt::Match { arms, default, .. } => {
            !arms.is_empty()
                && arms.iter().all(|(_, body)| statement_always_returns(body))
                && default.as_ref().is_none_or(|body| statement_always_returns(body))
        }
        _ => false,
    }
}

fn constant_integer_value(expression: &Expr) -> Result<Option<i128>, String> {
    constant_integer_value_with_globals(expression, &HashMap::new())
}

fn constant_integer_value_with_globals(
    expression: &Expr,
    globals: &HashMap<String, (Type, String)>,
) -> Result<Option<i128>, String> {
    match expression {
        Expr::Number(text) if !is_float_literal(text) => {
            let value: u128 = text.parse()
                .map_err(|_| format!("integer constant '{text}' is outside the supported range"))?;
            if value > i128::MAX as u128 {
                return Err(format!("integer constant '{text}' is outside the supported range"));
            }
            Ok(Some(value as i128))
        }
        Expr::Unary { op, right } => {
            let Some(value) = constant_integer_value_with_globals(right, globals)? else {
                return Ok(None);
            };
            let result = match op.as_str() {
                "-" => value.checked_neg(),
                "~" => Some(!value),
                "+" => Some(value),
                _ => return Ok(None),
            };
            result.map(Some).ok_or_else(|| "integer constant expression overflow".to_owned())
        }
        Expr::Variable(name) => {
            if let Some((ty, value)) = globals.get(name) {
                if ty.is_integer() || matches!(ty, Type::Enum(_)) {
                    return Ok(value.parse::<i128>().ok());
                }
            }
            Ok(None)
        }
        Expr::Binary { left, op, right } => {
            let (Some(left), Some(right)) = (
                constant_integer_value_with_globals(left, globals)?,
                constant_integer_value_with_globals(right, globals)?,
            ) else {
                return Ok(None);
            };
            let result = match op.as_str() {
                "+" => left.checked_add(right),
                "-" => left.checked_sub(right),
                "*" => left.checked_mul(right),
                "/" => left.checked_div(right),
                "%" => left.checked_rem(right),
                "&" => Some(left & right),
                "|" => Some(left | right),
                "^" => Some(left ^ right),
                "<<" if (0..128).contains(&right) => {
                    if right == 127 {
                        if left == 0 { Some(0) } else { None }
                    } else {
                        left.checked_mul(1_i128 << (right as u32))
                    }
                }
                ">>" if (0..128).contains(&right) => Some(left >> (right as u32)),
                "<<" | ">>" => return Err("integer constant shift count must be between 0 and 127".to_owned()),
                _ => return Ok(None),
            };
            result.map(Some).ok_or_else(|| {
                if matches!(op.as_str(), "/" | "%") && right == 0 {
                    "division by zero in integer constant expression".to_owned()
                } else {
                    "integer constant expression overflow".to_owned()
                }
            })
        }
        _ => Ok(None),
    }
}

fn constant_bool_value_with_globals(
    expression: &Expr,
    globals: &HashMap<String, (Type, String)>,
) -> Result<Option<bool>, String> {
    match expression {
        Expr::Bool(value) => Ok(Some(*value)),
        Expr::Variable(name) => {
            if let Some((Type::Bool, value)) = globals.get(name) {
                match value.as_str() {
                    "true" => Ok(Some(true)),
                    "false" => Ok(Some(false)),
                    _ => Ok(None),
                }
            } else {
                Ok(None)
            }
        }
        Expr::Unary { op, right } if op == "!" => {
            Ok(constant_bool_value_with_globals(right, globals)?.map(|value| !value))
        }
        Expr::Binary { left, op, right } if matches!(op.as_str(), "&&" | "||") => {
            let (Some(left), Some(right)) = (
                constant_bool_value_with_globals(left, globals)?,
                constant_bool_value_with_globals(right, globals)?,
            ) else {
                return Ok(None);
            };
            Ok(Some(if op == "&&" { left && right } else { left || right }))
        }
        Expr::Binary { left, op, right } if matches!(op.as_str(), "==" | "!=" | "<" | ">" | "<=" | ">=") => {
            if let (Some(left), Some(right)) = (
                constant_integer_value_with_globals(left, globals)?,
                constant_integer_value_with_globals(right, globals)?,
            ) {
                return Ok(Some(match op.as_str() {
                    "==" => left == right,
                    "!=" => left != right,
                    "<" => left < right,
                    ">" => left > right,
                    "<=" => left <= right,
                    ">=" => left >= right,
                    _ => unreachable!(),
                }));
            }
            if let (Some(left), Some(right)) = (
                constant_bool_value_with_globals(left, globals)?,
                constant_bool_value_with_globals(right, globals)?,
            ) {
                return Ok(Some(match op.as_str() {
                    "==" => left == right,
                    "!=" => left != right,
                    "<" => left < right,
                    ">" => left > right,
                    "<=" => left <= right,
                    ">=" => left >= right,
                    _ => unreachable!(),
                }));
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

fn constant_float_value_with_globals(
    expression: &Expr,
    globals: &HashMap<String, (Type, String)>,
) -> Result<Option<f64>, String> {
    match expression {
        Expr::Variable(name) => {
            if let Some((Type::F32 | Type::F64, value)) = globals.get(name) {
                return value.parse::<f64>().map(Some)
                    .map_err(|_| format!("floating-point constant '{name}' is invalid"));
            }
            Ok(None)
        }
        Expr::Number(text) => {
            let value: f64 = text.parse()
                .map_err(|_| format!("floating-point constant '{text}' is outside the supported range"))?;
            if !value.is_finite() {
                return Err(format!("floating-point constant '{text}' must be finite"));
            }
            Ok(Some(value))
        }
        Expr::Unary { op, right } => {
            let Some(value) = constant_float_value_with_globals(right, globals)? else {
                return Ok(None);
            };
            let result = match op.as_str() {
                "-" => -value,
                "+" => value,
                _ => return Ok(None),
            };
            if !result.is_finite() {
                return Err("floating-point constant expression is outside the finite range".to_owned());
            }
            Ok(Some(result))
        }
        Expr::Binary { left, op, right } if matches!(op.as_str(), "+" | "-" | "*" | "/") => {
            let (Some(left), Some(right)) = (
                constant_float_value_with_globals(left, globals)?,
                constant_float_value_with_globals(right, globals)?,
            ) else {
                return Ok(None);
            };
            let result = match op.as_str() {
                "+" => left + right,
                "-" => left - right,
                "*" => left * right,
                "/" => left / right,
                _ => unreachable!(),
            };
            if !result.is_finite() {
                return Err("floating-point constant expression is outside the finite range".to_owned());
            }
            Ok(Some(result))
        }
        _ => Ok(None),
    }
}

fn lower_global_constant(
    constant: &GlobalConst,
    globals: &HashMap<String, (Type, String)>,
) -> Result<(String, Option<String>), String> {
    let name = &constant.name;
    let mismatch = || format!(
        "global constant '{name}' requires a literal compatible with type {}",
        constant.ty.llvm()
    );

    if matches!(constant.ty, Type::Ptr | Type::Str | Type::Array(_, _) | Type::Struct(_))
        && matches!(&constant.value, Expr::Null)
    {
        return Ok(("null".to_owned(), None));
    }

    if matches!(constant.ty, Type::Ptr | Type::Array(_, _) | Type::Struct(_)) {
        if let Expr::Cast { value, target } = &constant.value {
            if matches!(target, Type::Ptr | Type::Struct(_)) {
                let Some(address) = constant_integer_value_with_globals(value, globals)? else {
                    return Err(format!("global pointer constant '{name}' requires an integer address literal"));
                };
                return Ok((format!("inttoptr (i64 {address} to ptr)"), None));
            }
        }
    }

    if constant.ty == Type::Str {
        if let Expr::String(value) = &constant.value {
            let encoded = value.as_bytes().iter()
                .map(|byte| format!("\\{:02X}", byte))
                .collect::<String>();
            let length = value.as_bytes().len() + 1;
            let declaration = format!(
                "@{name} = private unnamed_addr constant [{length} x i8] c\"{encoded}\\00\", align 1"
            );
            let operand = format!(
                "getelementptr inbounds ([{length} x i8], ptr @{name}, i64 0, i64 0)"
            );
            return Ok((operand, Some(declaration)));
        }
        return Err(mismatch());
    }

    if constant.ty == Type::Bool {
        if let Some(value) = constant_bool_value_with_globals(&constant.value, globals)? {
            return Ok((if value { "true" } else { "false" }.to_owned(), None));
        }
        return Err(mismatch());
    }

    if constant.ty == Type::F32 {
        if let Some(value) = constant_float_value_with_globals(&constant.value, globals)? {
            let single = value as f32;
            if !single.is_finite() {
                return Err(format!("floating-point constant '{name}' is outside the f32 finite range"));
            }
            return Ok((llvm_f32_constant(single), None));
        }
        return Err(mismatch());
    }

    if constant.ty == Type::F64 {
        if let Some(value) = constant_float_value_with_globals(&constant.value, globals)? {
            return Ok((format!("{value:.17e}"), None));
        }
        return Err(mismatch());
    }

    if constant.ty.is_integer() {
        let Some(value) = constant_integer_value_with_globals(&constant.value, globals)? else {
            return Err(mismatch());
        };
        let fits = match constant.ty {
            Type::I8 => (i8::MIN as i128..=i8::MAX as i128).contains(&value),
            Type::I16 => (i16::MIN as i128..=i16::MAX as i128).contains(&value),
            Type::I32 => (i32::MIN as i128..=i32::MAX as i128).contains(&value),
            Type::I64 => (i64::MIN as i128..=i64::MAX as i128).contains(&value),
            Type::U8 => (0..=u8::MAX as i128).contains(&value),
            Type::U16 => (0..=u16::MAX as i128).contains(&value),
            Type::U32 => (0..=u32::MAX as i128).contains(&value),
            Type::U64 => (0..=u64::MAX as i128).contains(&value),
            Type::Enum(_) => (i32::MIN as i128..=i32::MAX as i128).contains(&value),
            _ => false,
        };
        if !fits {
            return Err(mismatch());
        }
        return Ok((value.to_string(), None));
    }

    Err(format!(
        "global constant '{name}' has unsupported initializer; use a scalar, bool, string, or null literal"
    ))
}

fn resolve_enum_type(
    ty: &mut Type,
    enum_ids: &HashSet<u64>,
    alias_types: &HashMap<u64, Type>,
) -> Result<(), String> {
    if let Type::Future(output) = ty {
        let mut output_type = output.ty();
        resolve_enum_type(&mut output_type, enum_ids, alias_types)?;
        *output = FutureOutput::from_type(output_type)
            .ok_or_else(|| "unsupported Future output type after alias resolution".to_owned())?;
    }
    if let Type::Struct(type_id) = ty {
        if let Some(alias_type) = alias_types.get(type_id) {
            *ty = *alias_type;
        }
    }
    if let Type::Struct(type_id) = ty {
        if enum_ids.contains(type_id) {
            *ty = Type::Enum(*type_id);
        }
    }
    if let Type::Array(element, _) = ty {
        if let ArrayElement::Alias(type_id) = *element {
            let resolved = if let Some(alias_type) = alias_types.get(&type_id) {
                ArrayElement::from_resolved_type(*alias_type)
            } else if enum_ids.contains(&type_id) {
                Some(ArrayElement::I32)
            } else {
                None
            };
            let Some(resolved) = resolved else {
                return Err(format!(
                    "array element type 'type id {type_id:016x}' must resolve to a scalar or pointer type"
                ));
            };
            *element = resolved;
        }
    }
    Ok(())
}

fn resolve_global_enum_variant(expression: &mut Expr, enums: &[EnumDef]) -> Result<(), String> {
    if let Expr::Field { base, field } = expression {
        if let Expr::Variable(enum_name) = base.as_ref() {
            if let Some(enumeration) = enums.iter().find(|enumeration| enumeration.name == *enum_name) {
                let Some((_, value)) = enumeration.variants.iter().find(|(variant, _)| variant == field) else {
                    return Err(format!("enum '{}' has no variant '{}'", enumeration.name, field));
                };
                *expression = Expr::Number(value.to_string());
            }
        }
    }
    Ok(())
}

fn resolve_enum_expr(
    expression: &mut Expr,
    enum_ids: &HashSet<u64>,
    alias_types: &HashMap<u64, Type>,
) -> Result<(), String> {
    match expression {
        Expr::ArrayLiteral(elements) => {
            for element in elements {
                resolve_enum_expr(element, enum_ids, alias_types)?;
            }
        }
        Expr::StructLiteral { type_id, fields } => {
            if let Some(Type::Struct(resolved_id)) = alias_types.get(type_id) {
                *type_id = *resolved_id;
            }
            for (_, value) in fields {
                resolve_enum_expr(value, enum_ids, alias_types)?;
            }
        }
        Expr::SizeOf(ty) | Expr::AlignOf(ty) => resolve_enum_type(ty, enum_ids, alias_types)?,
        Expr::OffsetOf(ty, _) => resolve_enum_type(ty, enum_ids, alias_types)?,
        Expr::Field { base, .. } => resolve_enum_expr(base, enum_ids, alias_types)?,
        Expr::Unary { right, .. } => resolve_enum_expr(right, enum_ids, alias_types)?,
        Expr::Cast { value, target } => {
            resolve_enum_expr(value, enum_ids, alias_types)?;
            resolve_enum_type(target, enum_ids, alias_types)?;
        }
        Expr::Index { array, index } => {
            resolve_enum_expr(array, enum_ids, alias_types)?;
            resolve_enum_expr(index, enum_ids, alias_types)?;
        }
        Expr::Binary { left, right, .. } => {
            resolve_enum_expr(left, enum_ids, alias_types)?;
            resolve_enum_expr(right, enum_ids, alias_types)?;
        }
        Expr::Call { args, .. } => {
            for arg in args {
                resolve_enum_expr(arg, enum_ids, alias_types)?;
            }
        }
        Expr::Match { value, arms, default } => {
            resolve_enum_expr(value, enum_ids, alias_types)?;
            for (pattern, result) in arms {
                resolve_enum_expr(pattern, enum_ids, alias_types)?;
                resolve_enum_expr(result, enum_ids, alias_types)?;
            }
            if let Some(default) = default {
                resolve_enum_expr(default, enum_ids, alias_types)?;
            }
        }
        Expr::Number(_) | Expr::String(_) | Expr::Bool(_) | Expr::Null | Expr::Variable(_) => {}
    }
    Ok(())
}

fn resolve_enum_stmt(
    statement: &mut Stmt,
    enum_ids: &HashSet<u64>,
    alias_types: &HashMap<u64, Type>,
) -> Result<(), String> {
    match statement {
        Stmt::Block(statements) => {
            for statement in statements {
                resolve_enum_stmt(statement, enum_ids, alias_types)?;
            }
        }
        Stmt::Let { declared_type, value, .. } => {
            if let Some(ty) = declared_type {
                resolve_enum_type(ty, enum_ids, alias_types)?;
            }
            resolve_enum_expr(value, enum_ids, alias_types)?;
        }
        Stmt::Assign { value, .. } => resolve_enum_expr(value, enum_ids, alias_types)?,
        Stmt::FieldAssign { base, value, .. } => {
            resolve_enum_expr(base, enum_ids, alias_types)?;
            resolve_enum_expr(value, enum_ids, alias_types)?;
        }
        Stmt::IndexAssign { array, index, value } => {
            resolve_enum_expr(array, enum_ids, alias_types)?;
            resolve_enum_expr(index, enum_ids, alias_types)?;
            resolve_enum_expr(value, enum_ids, alias_types)?;
        }
        Stmt::Return(value) => {
            if let Some(value) = value {
                resolve_enum_expr(value, enum_ids, alias_types)?;
            }
        }
        Stmt::Expression(expression) => resolve_enum_expr(expression, enum_ids, alias_types)?,
        Stmt::If { condition, then_branch, else_branch } => {
            resolve_enum_expr(condition, enum_ids, alias_types)?;
            resolve_enum_stmt(then_branch, enum_ids, alias_types)?;
            if let Some(else_branch) = else_branch {
                resolve_enum_stmt(else_branch, enum_ids, alias_types)?;
            }
        }
        Stmt::Match { value, arms, default } => {
            resolve_enum_expr(value, enum_ids, alias_types)?;
            for (pattern, body) in arms {
                resolve_enum_expr(pattern, enum_ids, alias_types)?;
                resolve_enum_stmt(body, enum_ids, alias_types)?;
            }
            if let Some(default) = default {
                resolve_enum_stmt(default, enum_ids, alias_types)?;
            }
        }
        Stmt::While { condition, body } => {
            resolve_enum_expr(condition, enum_ids, alias_types)?;
            resolve_enum_stmt(body, enum_ids, alias_types)?;
        }
        Stmt::For { initializer, condition, increment, body } => {
            resolve_enum_stmt(initializer, enum_ids, alias_types)?;
            resolve_enum_expr(condition, enum_ids, alias_types)?;
            resolve_enum_stmt(increment, enum_ids, alias_types)?;
            resolve_enum_stmt(body, enum_ids, alias_types)?;
        }
        Stmt::Break | Stmt::Continue => {}
    }
    Ok(())
}

fn resolve_alias_target(
    ty: Type,
    aliases: &HashMap<u64, &TypeAlias>,
    structures: &HashMap<u64, &StructDef>,
    enums: &HashMap<u64, &EnumDef>,
    type_names: &HashMap<u64, String>,
    stack: &mut Vec<u64>,
) -> Result<Type, String> {
    match ty {
        Type::Struct(type_id) => {
            if let Some(alias) = aliases.get(&type_id) {
                if stack.contains(&type_id) {
                    return Err(format!("cyclic type alias involving '{}'", alias.name));
                }
                stack.push(type_id);
                let resolved = resolve_alias_target(
                    alias.target, aliases, structures, enums, type_names, stack,
                );
                stack.pop();
                resolved
            } else if enums.contains_key(&type_id) {
                Ok(Type::Enum(type_id))
            } else if structures.contains_key(&type_id) {
                Ok(Type::Struct(type_id))
            } else {
                let name = type_names.get(&type_id).cloned()
                    .unwrap_or_else(|| format!("type id {type_id:016x}"));
                Err(format!("unknown type '{name}'"))
            }
        }
        other => Ok(other),
    }
}

fn resolve_program_enum_types(source: &Program) -> Result<Program, String> {
    let mut program = source.clone();
    let structures = program.structs.iter().map(|item| (item.id, item)).collect::<HashMap<_, _>>();
    let mut enums = HashMap::<u64, &EnumDef>::new();
    let mut enum_names = HashMap::<String, u64>::new();
    for enumeration in &program.enums {
        if structures.contains_key(&enumeration.id) {
            return Err(format!("type name '{}' conflicts with a struct declaration", enumeration.name));
        }
        if enums.insert(enumeration.id, enumeration).is_some()
            || enum_names.insert(enumeration.name.clone(), enumeration.id).is_some()
        {
            return Err(format!("duplicate enum declaration '{}'", enumeration.name));
        }
    }

    let mut aliases = HashMap::<u64, &TypeAlias>::new();
    let mut alias_names = HashMap::<String, u64>::new();
    for alias in &program.type_aliases {
        if structures.contains_key(&alias.id) || enums.contains_key(&alias.id) {
            return Err(format!("type alias '{}' conflicts with an existing type declaration", alias.name));
        }
        if aliases.insert(alias.id, alias).is_some()
            || alias_names.insert(alias.name.clone(), alias.id).is_some()
        {
            return Err(format!("duplicate type alias '{}'", alias.name));
        }
    }

    let mut type_names = program.type_references.clone();
    for structure in &program.structs {
        type_names.insert(structure.id, structure.name.clone());
    }
    for enumeration in &program.enums {
        type_names.insert(enumeration.id, enumeration.name.clone());
    }
    for alias in &program.type_aliases {
        type_names.insert(alias.id, alias.name.clone());
    }

    for (type_id, type_name) in &program.type_references {
        if let Some(structure) = structures.get(type_id) {
            if structure.name != *type_name {
                return Err(format!("struct type identifier collision between '{}' and '{}'", structure.name, type_name));
            }
        } else if let Some(enumeration) = enums.get(type_id) {
            if enumeration.name != *type_name {
                return Err(format!("enum type identifier collision between '{}' and '{}'", enumeration.name, type_name));
            }
        } else if let Some(alias) = aliases.get(type_id) {
            if alias.name != *type_name {
                return Err(format!("type alias identifier collision between '{}' and '{}'", alias.name, type_name));
            }
        } else {
            return Err(format!("unknown type '{type_name}'"));
        }
    }

    let mut alias_types = HashMap::<u64, Type>::new();
    for alias in &program.type_aliases {
        let target = resolve_alias_target(
            alias.target, &aliases, &structures, &enums, &type_names, &mut Vec::new(),
        )?;
        alias_types.insert(alias.id, target);
    }

    let enum_ids = enums.keys().copied().collect::<HashSet<_>>();
    for structure in &mut program.structs {
        for (_, ty) in &mut structure.fields {
            resolve_enum_type(ty, &enum_ids, &alias_types)?;
        }
    }
    for function in &mut program.functions {
        for (_, ty) in &mut function.params {
            resolve_enum_type(ty, &enum_ids, &alias_types)?;
        }
        resolve_enum_type(&mut function.return_type, &enum_ids, &alias_types)?;
        if let Some(body) = &mut function.body {
            resolve_enum_stmt(body, &enum_ids, &alias_types)?;
        }
    }
    for constant in &mut program.constants {
        resolve_enum_type(&mut constant.ty, &enum_ids, &alias_types)?;
        resolve_global_enum_variant(&mut constant.value, &program.enums)?;
        resolve_enum_expr(&mut constant.value, &enum_ids, &alias_types)?;
    }
    for variable in &mut program.static_vars {
        resolve_enum_type(&mut variable.ty, &enum_ids, &alias_types)?;
        if let Some(value) = &mut variable.value {
            resolve_global_enum_variant(value, &program.enums)?;
            resolve_enum_expr(value, &enum_ids, &alias_types)?;
        }
    }
    // Alias references have already been replaced in every type position; they are
    // declarations, not LLVM struct/enum tags, so keep them out of backend tag validation.
    program.type_references.retain(|type_id, _| !aliases.contains_key(type_id));
    Ok(program)
}

#[cfg(test)]
fn compile_to_llvm(program: &Program) -> Result<String, String> {
    compile_to_llvm_for_target(program, usize::BITS, None)
}

fn async_abi_parameter_type(ty: Type) -> bool {
    matches!(
        ty,
        Type::I8 | Type::I16 | Type::I32 | Type::I64
            | Type::U8 | Type::U16 | Type::U32 | Type::U64
            | Type::F32 | Type::F64 | Type::Bool | Type::Enum(_)
    )
}

fn async_abi_result_type(ty: Type) -> bool {
    ty == Type::Void || async_abi_parameter_type(ty)
}

fn compile_to_llvm_for_target(
    program: &Program,
    target_pointer_width: u32,
    target_triple: Option<&str>,
) -> Result<String, String> {
    if !matches!(target_pointer_width, 32 | 64) {
        return Err(format!("unsupported target pointer width {target_pointer_width}; only 32-bit and 64-bit targets are supported"));
    }
    let normalized_program = resolve_program_enum_types(program)?;
    let program = &normalized_program;
    let functions = &program.functions;
    let mut structures = HashMap::<u64, StructDef>::new();
    for structure in &program.structs {
        if structures.insert(structure.id, structure.clone()).is_some() {
            return Err(format!("duplicate struct declaration '{}'", structure.name));
        }
    }
    let mut enumerations = HashMap::<u64, EnumDef>::new();
    for enumeration in &program.enums {
        if structures.contains_key(&enumeration.id) {
            return Err(format!("enum '{}' conflicts with a struct declaration", enumeration.name));
        }
        if enumerations.insert(enumeration.id, enumeration.clone()).is_some() {
            return Err(format!("duplicate enum declaration '{}'", enumeration.name));
        }
    }
    for (type_id, type_name) in &program.type_references {
        if let Some(structure) = structures.get(type_id) {
            if structure.name != *type_name {
                return Err(format!("struct type identifier collision between '{}' and '{}'", structure.name, type_name));
            }
        } else if let Some(enumeration) = enumerations.get(type_id) {
            if enumeration.name != *type_name {
                return Err(format!("enum type identifier collision between '{}' and '{}'", enumeration.name, type_name));
            }
        } else {
            return Err(format!("unknown type '{type_name}'"));
        }
    }

    let mut signatures = HashMap::new();
    for function in functions {
        if function.name == "printf"
            && (!function.external
                || !function.variadic
                || function.params.len() != 1
                || function.params[0].1 != Type::Str
                || function.return_type != Type::I32)
        {
            return Err("printf is a reserved runtime symbol; declare it as extern fn printf(format: str, ...) -> i32".to_owned());
        }
        if function.asynchronous {
            if function.external {
                return Err(format!("async function '{}' cannot be external", function.name));
            }
            if !async_abi_result_type(function.return_type) {
                return Err(format!(
                    "async function '{}' must return void, an integer, bool, an enum, or a float in this backend",
                    function.name
                ));
            }
            for (parameter, ty) in &function.params {
                if !async_abi_parameter_type(*ty) {
                    return Err(format!(
                        "async function '{}' parameter '{}' must be an integer, bool, enum, or float in this backend",
                        function.name, parameter
                    ));
                }
            }
        }
        let signature = Signature {
            params: function.params.iter().map(|(_, ty)| *ty).collect(),
            return_type: function.return_type,
            variadic: function.variadic,
            asynchronous: function.asynchronous,
        };
        if signatures.insert(function.name.clone(), signature).is_some() {
            return Err(format!("duplicate function '{}'", function.name));
        }
    }

    let mut global_values = HashMap::<String, (Type, String)>::new();
    let mut global_bindings = HashMap::<String, Binding>::new();
    let mut global_declarations = Vec::<String>::new();
    for constant in &program.constants {
        if signatures.contains_key(&constant.name) {
            return Err(format!("global constant '{}' conflicts with a function", constant.name));
        }
        if matches!(constant.name.as_str(), "printf" | "malloc" | "calloc" | "realloc" | "free" | "memcpy" | "memmove" | "memset" | "memcmp" | "strlen" | "strcmp" | "strncmp" | "memchr" | "llvm.trap") {
            return Err(format!("global constant '{}' conflicts with a reserved runtime symbol", constant.name));
        }
        if global_values.contains_key(&constant.name) || global_bindings.contains_key(&constant.name) {
            return Err(format!("duplicate global constant '{}'", constant.name));
        }
        let (value, declaration) = lower_global_constant(constant, &global_values)?;
        if let Some(declaration) = declaration {
            global_declarations.push(declaration);
        } else {
            global_declarations.push(format!(
                "@{} = constant {} {}",
                constant.name, constant.ty.llvm(), value
            ));
        }
        global_values.insert(constant.name.clone(), (constant.ty, value));
    }

    for variable in &program.static_vars {
        if signatures.contains_key(&variable.name) {
            return Err(format!("static variable '{}' conflicts with a function", variable.name));
        }
        if matches!(variable.name.as_str(), "printf" | "malloc" | "free" | "memcpy" | "memmove" | "memset" | "memcmp" | "llvm.trap") {
            return Err(format!("static variable '{}' conflicts with a reserved runtime symbol", variable.name));
        }
        if global_values.contains_key(&variable.name) || global_bindings.contains_key(&variable.name) {
            return Err(format!("duplicate global symbol '{}'", variable.name));
        }
        if variable.external {
            let storage = if variable.mutable { "global" } else { "constant" };
            global_declarations.push(format!(
                "@{} = external {storage} {}",
                variable.name, variable.ty.llvm()
            ));
        } else {
            let initializer_value = variable.value.as_ref()
                .ok_or_else(|| format!("static variable '{}' is missing an initializer", variable.name))?;
            if let (Type::Array(element, length), Expr::ArrayLiteral(elements)) = (variable.ty, initializer_value) {
                if elements.len() != length as usize {
                    return Err(format!(
                        "static array '{}' has {} initializer element(s), but its type requires {length}",
                        variable.name, elements.len()
                    ));
                }
                let element_type = element.ty();
                let aggregate_type = format!("[{length} x {}]", element.llvm());
                let storage_name = format!(".ramlang.static.storage.{}", variable.name);
                let mut initializers = Vec::with_capacity(elements.len());
                for (index, expression) in elements.iter().enumerate() {
                    let initializer = GlobalConst {
                        name: format!(".ramlang.static.{}.element.{index}", variable.name),
                        ty: element_type,
                        value: expression.clone(),
                    };
                    let (value, declaration) = lower_global_constant(&initializer, &global_values)?;
                    if let Some(declaration) = declaration {
                        global_declarations.push(declaration);
                    }
                    initializers.push(format!("{} {value}", element.llvm()));
                }
                let storage = if variable.mutable { "global" } else { "constant" };
                global_declarations.push(format!(
                    "@{storage_name} = {storage} {aggregate_type} [{}]",
                    initializers.join(", ")
                ));
                global_declarations.push(format!(
                    "@{} = constant ptr getelementptr inbounds ({aggregate_type}, ptr @{storage_name}, i64 0, i64 0)",
                    variable.name
                ));
            } else {
                let initializer_name = if variable.ty == Type::Str && matches!(initializer_value, Expr::String(_)) {
                    format!(".ramlang.static.{}", variable.name)
                } else {
                    variable.name.clone()
                };
                let initializer = GlobalConst {
                    name: initializer_name,
                    ty: variable.ty,
                    value: initializer_value.clone(),
                };
                let (value, declaration) = lower_global_constant(&initializer, &global_values)?;
                if let Some(declaration) = declaration {
                    global_declarations.push(declaration);
                }
                let storage = if variable.mutable { "global" } else { "constant" };
                global_declarations.push(format!(
                    "@{} = {storage} {} {}",
                    variable.name, variable.ty.llvm(), value
                ));
            }
        }
        global_bindings.insert(variable.name.clone(), Binding {
            ty: variable.ty,
            slot: format!("@{}", variable.name),
            mutable: variable.mutable,
            read_only_pointee: !variable.mutable
                && matches!(variable.ty, Type::Array(_, _))
                && matches!(variable.value.as_ref(), Some(Expr::ArrayLiteral(_))),
        });
    }

    let main = signatures.get("main").ok_or_else(|| "program must define fn main() -> i32".to_owned())?;
    let main_function = functions.iter().find(|function| function.name == "main").unwrap();
    if main_function.external || main_function.asynchronous || !main.params.is_empty() || main.return_type != Type::I32 {
        return Err("entry point must be a synchronous definition with signature fn main() -> i32".to_owned());
    }
    for function in functions {
        if let Some(body) = &function.body {
            if function.return_type != Type::Void && !statement_always_returns(body) {
                return Err(format!("function '{}' must return a value on every path", function.name));
            }
        }
    }

    let mut module = String::from("; RamLang native module generated by ramcomp\nsource_filename = \"ramlang\"\n");
    if let Some(target_triple) = target_triple {
        module.push_str(&format!("target triple = \"{target_triple}\"\n"));
    }
    module.push('\n');
    for structure in &program.structs {
        let fields = structure.fields.iter()
            .map(|(_, ty)| ty.llvm())
            .collect::<Vec<_>>()
            .join(", ");
        if structure.packed {
            module.push_str(&format!("%struct.{:016x} = type <{{ {fields} }}>\n", structure.id));
        } else {
            module.push_str(&format!("%struct.{:016x} = type {{ {fields} }}\n", structure.id));
        }
    }
    for function in functions.iter().filter(|function| function.asynchronous) {
        let id = struct_type_id(&function.name);
        let fields = if function.params.is_empty() {
            "i8".to_owned()
        } else {
            function.params.iter().map(|(_, ty)| ty.llvm()).collect::<Vec<_>>().join(", ")
        };
        module.push_str(&format!("%__ramlang_async_args_{id:016x} = type {{ {fields} }}\n"));
    }
    if !program.structs.is_empty() || functions.iter().any(|function| function.asynchronous) {
        module.push('\n');
    }
    for declaration in &global_declarations {
        module.push_str(declaration);
        module.push('\n');
    }
    if !global_declarations.is_empty() {
        module.push('\n');
    }
    module.push_str("@.fmt.i32 = private unnamed_addr constant [4 x i8] [i8 37, i8 100, i8 10, i8 0], align 1\n");
    module.push_str("@.fmt.i64 = private unnamed_addr constant [6 x i8] [i8 37, i8 108, i8 108, i8 100, i8 10, i8 0], align 1\n");
    module.push_str("@.fmt.u32 = private unnamed_addr constant [4 x i8] [i8 37, i8 117, i8 10, i8 0], align 1\n");
    module.push_str("@.fmt.u64 = private unnamed_addr constant [6 x i8] [i8 37, i8 108, i8 108, i8 117, i8 10, i8 0], align 1\n");
    module.push_str("@.fmt.f64 = private unnamed_addr constant [4 x i8] [i8 37, i8 103, i8 10, i8 0], align 1\n");
    module.push_str("@.fmt.str = private unnamed_addr constant [4 x i8] [i8 37, i8 115, i8 10, i8 0], align 1\n");
    module.push_str("declare i32 @printf(ptr, ...)\n\n");
    let size_t = if target_pointer_width == 32 { "i32" } else { "i64" };
    module.push_str(&format!("declare ptr @malloc({size_t})\n"));
    module.push_str(&format!("declare ptr @calloc({size_t}, {size_t})\n"));
    module.push_str(&format!("declare ptr @realloc(ptr, {size_t})\n"));
    module.push_str("declare void @free(ptr)\n");
    module.push_str(&format!("declare ptr @memcpy(ptr, ptr, {size_t})\n"));
    module.push_str(&format!("declare ptr @memmove(ptr, ptr, {size_t})\n"));
    module.push_str(&format!("declare ptr @memset(ptr, i32, {size_t})\n"));
    module.push_str(&format!("declare i32 @memcmp(ptr, ptr, {size_t})\n"));
    module.push_str(&format!("declare {size_t} @strlen(ptr)\n"));
    module.push_str("declare i32 @strcmp(ptr, ptr)\n");
    module.push_str(&format!("declare i32 @strncmp(ptr, ptr, {size_t})\n"));
    module.push_str(&format!("declare ptr @memchr(ptr, i32, {size_t})\n"));
    module.push_str("declare ptr @ramlang_alloc(i64)\n");
    module.push_str("declare void @ramlang_free(ptr)\n");
    module.push_str("declare ptr @ramlang_future_spawn_i64(ptr, ptr)\n");
    module.push_str("declare i64 @ramlang_future_await_i64(ptr)\n");
    module.push_str("declare void @ramlang_future_close(ptr)\n");
    module.push_str("declare void @llvm.trap() cold noreturn nounwind\n");
    module.push_str("declare void @llvm.assume(i1) nounwind\n");

    for llvm_type in ["i8", "i16", "i32", "i64"] {
        for intrinsic in ["sadd", "ssub", "smul", "uadd", "usub", "umul"] {
            module.push_str(&format!(
                "declare {{ {llvm_type}, i1 }} @llvm.{intrinsic}.with.overflow.{llvm_type}({llvm_type}, {llvm_type})\n"
            ));
        }
        for intrinsic in ["sadd.sat", "ssub.sat", "uadd.sat", "usub.sat"] {
            module.push_str(&format!(
                "declare {llvm_type} @llvm.{intrinsic}.{llvm_type}({llvm_type}, {llvm_type})\n"
            ));
        }
        module.push_str(&format!("declare {llvm_type} @llvm.ctpop.{llvm_type}({llvm_type})\n"));
        module.push_str(&format!("declare {llvm_type} @llvm.ctlz.{llvm_type}({llvm_type}, i1 immarg)\n"));
        module.push_str(&format!("declare {llvm_type} @llvm.cttz.{llvm_type}({llvm_type}, i1 immarg)\n"));
        module.push_str(&format!("declare {llvm_type} @llvm.bitreverse.{llvm_type}({llvm_type})\n"));
        if llvm_type != "i8" {
            module.push_str(&format!("declare {llvm_type} @llvm.bswap.{llvm_type}({llvm_type})\n"));
        }
    }
    module.push('\n');

    for function in functions {
        if function.external {
            if function.name == "printf" {
                // The runtime declaration emitted above is the canonical printf prototype.
                continue;
            }
            let mut params = function.params.iter()
                .map(|(_, ty)| ty.llvm())
                .collect::<Vec<_>>();
            if function.variadic {
                params.push("...");
            }
            module.push_str(&format!("declare {} @{}({})\n\n", function.return_type.llvm(), function.name, params.join(", ")));
            continue;
        }

        let params = function.params.iter().enumerate()
            .map(|(index, (_, ty))| format!("{} %arg{index}", ty.llvm()))
            .collect::<Vec<_>>()
            .join(", ");
        let generator = FunctionGenerator::new(&signatures, &structures, &enumerations, &global_values, &global_bindings, target_pointer_width, function.return_type, &function.name);
        let (body, string_globals, _) = generator.generate(function)?;
        for global in string_globals {
            module.push_str(&global);
            module.push('\n');
        }
        let header = format!("define {} @{}({params}) {{\n", function.return_type.llvm(), function.name);
        module.push_str(&header);
        module.push_str(&body);
        module.push_str("}\n\n");
        if function.asynchronous {
            module.push_str(&emit_async_entry_wrapper(function));
            module.push_str("\n");
        }
    }
    Ok(module)
}

fn parse_source(source: &str) -> Result<Program, String> {
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
    emit_bitcode: bool,
    emit_header: bool,
    check_only: bool,
    link_libs: Vec<String>,
    link_search_paths: Vec<PathBuf>,
    link_args: Vec<String>,
    target: Option<String>,
    target_cpu: Option<String>,
    target_features: Vec<String>,
    sysroot: Option<PathBuf>,
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
        return Err("usage: ramcomp <source.ram> [additional.ram ...] [--check] [--emit-llvm [output.ll] | --emit-obj | --emit-asm | --emit-bc | --emit-header] [-o output] [--target TRIPLE] [--target-cpu CPU] [--target-feature FEATURE ...] [--sysroot PATH] [--opt-level LEVEL] [--link-lib NAME] [--link-search PATH] [--link-arg ARG]".to_owned());
    };
    if source == "--help" || source == "-h" {
        println!("ramcomp - RamLang LLVM compiler\n\nUSAGE:\n  ramcomp <source.ram> [additional.ram ...] [--check] [--emit-llvm [output.ll] | --emit-obj | --emit-asm | --emit-bc] [-o output] [--target TRIPLE] [--opt-level LEVEL] [--link-lib NAME] [--link-search PATH] [--link-arg ARG]\n\nOPTIONS:\n  --check                 Parse and type-check without producing an executable\n  --emit-llvm             Write LLVM IR instead of compiling a native executable\n  --emit-obj              Emit a native object file without linking\n  --emit-asm              Emit target assembly without linking\n  --emit-bc               Emit LLVM bitcode without linking\n  --emit-header           Generate a C header for callable functions and global symbols\n  --target <triple>       Select the Clang compilation target\n  --target-cpu <name>    Select the LLVM target CPU\n  --target-feature <f>   Enable (+) or disable (-) an LLVM target feature; repeatable\n  --sysroot <path>       Use a target sysroot when invoking Clang\n  --opt-level <level>     Optimization level: 0, 1, 2, 3, s, or z (default: 2)\n  -o <path>               Set output executable or IR path\n  -l, --link-lib <name>   Link a native library (for example, --link-lib m)\n  -L, --link-search <dir> Add a native library search directory\n  --link-arg <arg>        Pass an additional argument to Clang\n  -h, --help              Show this help\n\nNative compilation uses clang (LLVM 15 or newer). Set RAMLANG_CLANG to override its path.");
        process::exit(0);
    }

    let mut options = Options {
        sources: vec![PathBuf::from(source)],
        output: None,
        emit_llvm: false,
        emit_object: false,
        emit_assembly: false,
        emit_bitcode: false,
        emit_header: false,
        check_only: false,
        link_libs: Vec::new(),
        link_search_paths: Vec::new(),
        link_args: Vec::new(),
        target: None,
        target_cpu: None,
        target_features: Vec::new(),
        sysroot: None,
        opt_level: "2".to_owned(),
        opt_level_explicit: false,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--check" => options.check_only = true,
            "--emit-obj" | "--emit-object" => options.emit_object = true,
            "--emit-asm" => options.emit_assembly = true,
            "--emit-bc" | "--emit-bitcode" => options.emit_bitcode = true,
            "--emit-header" => {
                options.emit_header = true;
                if args.peek().is_some_and(|next| !next.starts_with('-')) {
                    options.output = args.next().map(PathBuf::from);
                }
            }
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
                if target.starts_with('-') || target.is_empty()
                    || !target.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '+'))
                {
                    return Err(format!("invalid target triple '{target}'"));
                }
                options.target = Some(target);
            }
            "--target-cpu" => {
                let Some(cpu) = args.next() else { return Err("expected a CPU name after --target-cpu".to_owned()); };
                if cpu.is_empty() || cpu.starts_with('-') {
                    return Err(format!("invalid target CPU '{cpu}'"));
                }
                options.target_cpu = Some(cpu);
            }
            "--target-feature" => {
                let Some(feature) = args.next() else { return Err("expected a feature such as +avx2 or -sse4.2 after --target-feature".to_owned()); };
                let valid_body = feature.get(1..).is_some_and(|body| !body.is_empty() && body.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')));
                if !matches!(feature.as_bytes().first(), Some(b'+') | Some(b'-')) || !valid_body {
                    return Err(format!("invalid target feature '{feature}'; expected +feature or -feature"));
                }
                options.target_features.push(feature);
            }
            "--sysroot" => {
                let Some(path) = args.next() else { return Err("expected a path after --sysroot".to_owned()); };
                if path.is_empty() {
                    return Err("sysroot path cannot be empty".to_owned());
                }
                options.sysroot = Some(PathBuf::from(path));
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
        + usize::from(options.emit_assembly)
        + usize::from(options.emit_bitcode)
        + usize::from(options.emit_header);
    if output_mode_count > 1 {
        return Err("--emit-llvm, --emit-obj, --emit-asm, --emit-bc, and --emit-header are mutually exclusive".to_owned());
    }
    if options.check_only && output_mode_count > 0 {
        return Err("--check cannot be combined with an output mode".to_owned());
    }
    let link_options_present = !options.link_libs.is_empty()
        || !options.link_search_paths.is_empty()
        || !options.link_args.is_empty();
    if options.check_only && options.opt_level_explicit {
        return Err("optimization options require native code generation and cannot be combined with --check".to_owned());
    }
    if options.emit_llvm && options.opt_level_explicit {
        return Err("optimization options cannot be combined with --emit-llvm".to_owned());
    }
    if (options.check_only || options.emit_llvm)
        && (options.target_cpu.is_some() || !options.target_features.is_empty() || options.sysroot.is_some())
    {
        return Err("target CPU, target features, and sysroot require backend code generation and cannot be combined with --check or --emit-llvm".to_owned());
    }
    if (options.check_only || output_mode_count > 0) && link_options_present {
        return Err("linker options require native linking and cannot be combined with --check or an emit-only mode".to_owned());
    }
    Ok(options)
}

fn target_pointer_width(target: Option<&str>) -> Result<u32, String> {
    let Some(target) = target else {
        return match usize::BITS {
            32 | 64 => Ok(usize::BITS),
            width => Err(format!("unsupported host pointer width {width}; only 32-bit and 64-bit hosts are supported")),
        };
    };
    let architecture = target.split('-').next().unwrap_or(target);
    if target.contains("gnux32") || target.contains("abi32") {
        return Ok(32);
    }
    match architecture {
        "x86_64" | "amd64" | "aarch64" | "arm64" | "riscv64"
        | "powerpc64" | "powerpc64le" | "s390x" | "mips64" | "mips64el"
        | "wasm64" | "loongarch64" | "sparc64" => Ok(64),
        "i386" | "i486" | "i586" | "i686" | "x86" | "arm" | "riscv32"
        | "wasm32" | "mips" | "mipsel" | "powerpc" | "powerpcle"
        | "sparc" | "hexagon" | "sh4" => Ok(32),
        arch if arch.starts_with("armv") || arch.starts_with("thumb") => Ok(32),
        _ => Err(format!(
            "cannot determine pointer width for target '{target}'; supported target triples must identify a known 32-bit or 64-bit architecture"
        )),
    }
}

fn merge_program(target: &mut Program, mut source: Program) -> Result<(), String> {
    target.functions.append(&mut source.functions);
    target.structs.append(&mut source.structs);
    target.enums.append(&mut source.enums);
    target.type_aliases.append(&mut source.type_aliases);
    target.constants.append(&mut source.constants);
    target.static_vars.append(&mut source.static_vars);
    for (type_id, type_name) in source.type_references {
        if let Some(existing) = target.type_references.get(&type_id) {
            if existing != &type_name {
                return Err(format!("type identifier collision between '{existing}' and '{type_name}'"));
            }
        } else {
            target.type_references.insert(type_id, type_name);
        }
    }
    Ok(())
}

fn load_source_recursive(
    source_path: &Path,
    visited: &mut HashSet<PathBuf>,
) -> Result<Vec<Program>, String> {
    let canonical_path = fs::canonicalize(source_path)
        .map_err(|error| format!("cannot resolve source/import path {}: {error}", source_path.display()))?;
    if !visited.insert(canonical_path.clone()) {
        return Ok(Vec::new());
    }

    let source = fs::read_to_string(&canonical_path)
        .map_err(|error| format!("cannot read {}: {error}", canonical_path.display()))?;
    let mut parsed = parse_source(&source)
        .map_err(|error| format!("{}: {error}", canonical_path.display()))?;
    let imports = std::mem::take(&mut parsed.imports);
    let base_directory = canonical_path.parent().unwrap_or_else(|| Path::new("."));
    let mut programs = Vec::new();
    for import in imports {
        let import_path = Path::new(&import);
        let resolved_import = if import_path.is_absolute() {
            import_path.to_path_buf()
        } else {
            base_directory.join(import_path)
        };
        programs.extend(load_source_recursive(&resolved_import, visited)?);
    }
    programs.push(parsed);
    Ok(programs)
}

fn is_c_keyword(name: &str) -> bool {
    matches!(name,
        "auto" | "break" | "case" | "char" | "const" | "continue" | "default" |
        "do" | "double" | "else" | "enum" | "extern" | "float" | "for" |
        "goto" | "if" | "inline" | "int" | "long" | "register" | "restrict" |
        "return" | "short" | "signed" | "sizeof" | "static" | "struct" |
        "switch" | "typedef" | "union" | "unsigned" | "void" | "volatile" |
        "while" | "_Alignas" | "_Alignof" | "_Atomic" | "_Bool" | "_Complex" |
        "_Generic" | "_Imaginary" | "_Noreturn" | "_Static_assert" | "_Thread_local"
    )
}

fn validate_c_identifier(name: &str, context: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let valid_start = chars.next().is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic());
    let valid_rest = chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric());
    if !valid_start || !valid_rest || is_c_keyword(name) {
        return Err(format!("cannot generate C header: {context} name '{name}' is not a valid C identifier"));
    }
    Ok(())
}

fn c_array_element_type(element: ArrayElement, program: &Program, depth: usize) -> Result<String, String> {
    if depth > 32 {
        return Err("cannot generate C header: recursive type alias".to_owned());
    }
    let ty = match element {
        ArrayElement::I8 => Type::I8,
        ArrayElement::I16 => Type::I16,
        ArrayElement::I32 => Type::I32,
        ArrayElement::I64 => Type::I64,
        ArrayElement::U8 => Type::U8,
        ArrayElement::U16 => Type::U16,
        ArrayElement::U32 => Type::U32,
        ArrayElement::U64 => Type::U64,
        ArrayElement::F32 => Type::F32,
        ArrayElement::F64 => Type::F64,
        ArrayElement::Bool => Type::Bool,
        ArrayElement::Ptr => Type::Ptr,
        ArrayElement::Str => Type::Str,
        ArrayElement::Alias(id) => Type::Struct(id),
    };
    c_type_name(ty, program, depth + 1)
}

fn c_type_name(ty: Type, program: &Program, depth: usize) -> Result<String, String> {
    if depth > 32 {
        return Err("cannot generate C header: recursive type alias".to_owned());
    }
    match ty {
        Type::I8 => Ok("int8_t".to_owned()),
        Type::I16 => Ok("int16_t".to_owned()),
        Type::I32 => Ok("int32_t".to_owned()),
        Type::I64 => Ok("int64_t".to_owned()),
        Type::U8 => Ok("uint8_t".to_owned()),
        Type::U16 => Ok("uint16_t".to_owned()),
        Type::U32 => Ok("uint32_t".to_owned()),
        Type::U64 => Ok("uint64_t".to_owned()),
        Type::F32 => Ok("float".to_owned()),
        Type::F64 => Ok("double".to_owned()),
        Type::Bool => Ok("bool".to_owned()),
        Type::Str => Ok("const char *".to_owned()),
        Type::Ptr | Type::Future(_) => Ok("void *".to_owned()),
        Type::Array(element, _) => Ok(format!("{} *", c_array_element_type(element, program, depth + 1)?)),
        Type::Enum(_) => Ok("int32_t".to_owned()),
        Type::Struct(id) => {
            if let Some(alias) = program.type_aliases.iter().find(|alias| alias.id == id) {
                c_type_name(alias.target, program, depth + 1)
            } else if program.enums.iter().any(|enumeration| enumeration.id == id) {
                Ok("int32_t".to_owned())
            } else if program.structs.iter().any(|structure| structure.id == id) {
                Ok("void *".to_owned())
            } else {
                Err(format!("cannot generate C header: unknown type identifier {id:016x}"))
            }
        }
        Type::Void => Ok("void".to_owned()),
    }
}

fn c_global_declaration_type(ty: Type, immutable: bool, program: &Program) -> Result<String, String> {
    let base = c_type_name(ty, program, 0)?;
    if !immutable {
        Ok(base)
    } else if base.contains('*') {
        Ok(format!("{base} const"))
    } else {
        Ok(format!("const {base}"))
    }
}

fn generate_c_header(program: &Program) -> Result<String, String> {
    let mut output = String::from(
        "#ifndef RAMLANG_GENERATED_API_H\n#define RAMLANG_GENERATED_API_H\n\n#include <stdbool.h>\n#include <stdint.h>\n\n#ifdef __cplusplus\nextern \"C\" {\n#endif\n\n"
    );
    for enumeration in &program.enums {
        for (variant, value) in &enumeration.variants {
            let macro_name = format!("RAMLANG_{}_{}", enumeration.name.to_ascii_uppercase(), variant.to_ascii_uppercase());
            output.push_str(&format!("#define {macro_name} ({value})\n"));
        }
    }
    if !program.enums.is_empty() {
        output.push('\n');
    }

    for function in &program.functions {
        if function.name == "main" || function.asynchronous {
            continue;
        }
        validate_c_identifier(&function.name, "function")?;
        let return_type = c_type_name(function.return_type, program, 0)?;
        let mut parameters = Vec::new();
        for (name, ty) in &function.params {
            validate_c_identifier(name, "parameter")?;
            if *ty == Type::Void {
                return Err(format!("cannot generate C header: parameter '{name}' of '{}' has type void", function.name));
            }
            parameters.push(format!("{} {name}", c_type_name(*ty, program, 0)?));
        }
        if function.variadic {
            parameters.push("...".to_owned());
        }
        if parameters.is_empty() {
            parameters.push("void".to_owned());
        }
        output.push_str(&format!("extern {return_type} {}({});\n", function.name, parameters.join(", ")));
    }

    if !program.functions.is_empty() {
        output.push('\n');
    }
    for constant in &program.constants {
        validate_c_identifier(&constant.name, "constant")?;
        let ty = c_global_declaration_type(constant.ty, true, program)?;
        output.push_str(&format!("extern {ty} {};\n", constant.name));
    }
    for variable in &program.static_vars {
        validate_c_identifier(&variable.name, "global variable")?;
        let ty = c_global_declaration_type(variable.ty, !variable.mutable, program)?;
        output.push_str(&format!("extern {ty} {};\n", variable.name));
    }

    output.push_str("\n#ifdef __cplusplus\n}\n#endif\n\n#endif /* RAMLANG_GENERATED_API_H */\n");
    Ok(output)
}

fn compile(options: Options) -> Result<(), String> {
    let mut program = Program::default();
    let mut visited = HashSet::<PathBuf>::new();
    for source_path in &options.sources {
        for parsed in load_source_recursive(source_path, &mut visited)? {
            merge_program(&mut program, parsed)?;
        }
    }
    let source_label = options.sources.iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let pointer_width = target_pointer_width(options.target.as_deref())?;
    let llvm_ir = compile_to_llvm_for_target(&program, pointer_width, options.target.as_deref())
        .map_err(|error| format!("{source_label}: {error}"))?;

    if options.check_only {
        println!("{source_label}: OK");
        return Ok(());
    }

    if options.emit_llvm {
        llvm_backend::verify_ir(&llvm_ir)?;
        let output = options.output.unwrap_or_else(|| options.sources[0].with_extension("ll"));
        fs::write(&output, llvm_ir)
            .map_err(|error| format!("cannot write {}: {error}", output.display()))?;
        println!("wrote {}", output.display());
        return Ok(());
    }

    if options.emit_header {
        let output = options.output.unwrap_or_else(|| options.sources[0].with_extension("h"));
        let header = generate_c_header(&program)?;
        fs::write(&output, header)
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
    } else if options.emit_bitcode {
        options.sources[0].with_extension("bc")
    } else {
        options.sources[0].with_extension("")
    };
    if options.emit_object || options.emit_assembly || options.emit_bitcode {
        let kind = if options.emit_object {
            llvm_backend::OutputKind::Object
        } else if options.emit_assembly {
            llvm_backend::OutputKind::Assembly
        } else {
            llvm_backend::OutputKind::Bitcode
        };
        llvm_backend::emit_file(
            &llvm_ir,
            &output,
            kind,
            options.target.as_deref(),
            options.target_cpu.as_deref(),
            &options.target_features,
            &options.opt_level,
        )?;
        println!("built {}", output.display());
        return Ok(());
    }

    // LLVM code generation happens in-process in Rust. The platform driver is used
    // only to link the resulting object with the system runtime and requested libraries.
    let object_path = env::temp_dir().join(format!("ramlang-{}-linked.o", process::id()));
    llvm_backend::emit_file(
        &llvm_ir,
        &object_path,
        llvm_backend::OutputKind::Object,
        options.target.as_deref(),
        options.target_cpu.as_deref(),
        &options.target_features,
        &options.opt_level,
    )?;

    let linker = env::var_os("RAMLANG_LINKER")
        .or_else(|| env::var_os("RAMLANG_CLANG"))
        .unwrap_or_else(|| "clang".into());
    let mut command = Command::new(&linker);
    let requires_async_runtime = program.functions.iter().any(|function| function.asynchronous);
    let runtime_library_dir = if requires_async_runtime {
        env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf))
    } else {
        None
    };
    if let Some(target) = &options.target {
        command.arg("-target").arg(target);
    }
    if let Some(sysroot) = &options.sysroot {
        command.arg("--sysroot").arg(sysroot);
    }
    for path in &options.link_search_paths {
        command.arg("-L").arg(path);
    }
    for library in &options.link_libs {
        command.arg(format!("-l{library}"));
    }
    for argument in &options.link_args {
        command.arg(argument);
    }
    if let Some(directory) = &runtime_library_dir {
        command.arg("-L").arg(directory);
        let target_triple = options.target.as_deref().unwrap_or("");
        if target_triple.contains("linux")
            || target_triple.contains("freebsd")
            || (target_triple.is_empty() && cfg!(unix))
        {
            command.arg(format!("-Wl,-rpath,{}", directory.display()));
        }
    }
    let runtime_library_is_explicit = options.link_libs.iter().any(|library| library == "ramlang_runtime");
    command.arg(&object_path);
    if requires_async_runtime && !runtime_library_is_explicit {
        command.arg("-lramlang_runtime");
    }
    let result = command.arg("-o").arg(&output).output();
    let _ = fs::remove_file(&object_path);
    let result = result.map_err(|error| {
        format!(
            "could not launch native linker ({:?}): {error}; install a compatible linker driver or set RAMLANG_LINKER",
            linker
        )
    })?;
    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        return Err(format!("native linker failed to link LLVM object:\n{stderr}"));
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
        let program = parse_source(source)?;
        compile_to_llvm(&program)
    }

    #[test]
    fn emits_integer_arithmetic() {
        let ir = compile("fn main() -> i32 { let x: i32 = 20 + 22; return x; }").unwrap();
        assert!(ir.contains("add i32 20, 22"));
        assert!(ir.contains("ret i32"));
    }

    #[test]
    fn guards_integer_division_and_remainder() {
        let source = "fn main() -> i32 { let numerator: i32 = 10; let denominator: i32 = 2; print(numerator / denominator); print(numerator % denominator); let unsigned: u32 = 8; print(unsigned / 2); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("icmp ne i32"));
        assert!(ir.contains("icmp eq i32"));
        assert!(ir.contains("icmp eq i32"));
        assert!(ir.contains("and i1"));
        assert!(ir.contains("integer.division.trap"));
        assert!(ir.contains("call void @llvm.trap()"));
        assert!(ir.contains("sdiv i32"));
        assert!(ir.contains("srem i32"));
        assert!(ir.contains("udiv i32"));
    }

    #[test]
    fn emits_guards_for_signed_minimum_divided_by_negative_one() {
        let ir = compile("fn main() -> i32 { let value: i32 = -2147483648; let minus_one: i32 = -1; return value / minus_one; }").unwrap();
        assert!(ir.contains("icmp eq i32"));
        assert!(ir.contains("-2147483648"));
        assert!(ir.contains("integer.division.trap"));
    }

    #[test]
    fn resolves_scalar_array_chained_and_struct_type_aliases() {
        let source = "type Scalar = i32; type Number = Scalar; type Values = [i32; 3]; struct Point { x: Number, y: Scalar } type Location = Point; fn add(left: Number, right: Scalar) -> Number { return left + right; } fn main() -> i32 { let values: Values = [10, 20, 12]; let point: Location = Location { x: 5, y: 7 }; print(add(values[2], point.x)); print(point.y); return 0; }";
        let ir = compile(source).unwrap();
        let point_id = struct_type_id("Point");
        assert!(ir.contains(&format!("%struct.{point_id:016x} = type {{ i32, i32 }}")));
        assert!(ir.contains("define i32 @add(i32"));
        assert!(ir.contains("alloca [3 x i32]"));
        assert!(ir.contains("store i32 12"));
    }

    #[test]
    fn resolves_type_aliases_across_imported_source_files() {
        let root = env::temp_dir().join(format!("ramlang-type-alias-import-test-{}", process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("types")).unwrap();
        fs::write(root.join("main.ram"), "import \"types/number.ram\"; fn main() -> i32 { let value: Word = 40; print(value + 2); return 0; }").unwrap();
        fs::write(root.join("types/number.ram"), "type Scalar = i32; type Word = Scalar;").unwrap();

        let mut program = Program::default();
        let mut visited = HashSet::new();
        for part in load_source_recursive(&root.join("main.ram"), &mut visited).unwrap() {
            merge_program(&mut program, part).unwrap();
        }
        assert!(compile_to_llvm(&program).is_ok());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_cyclic_duplicate_and_conflicting_type_aliases() {
        let cycle = compile("type First = Second; type Second = First; fn main() -> i32 { return 0; }").unwrap_err();
        assert!(cycle.contains("cyclic type alias"));
        let duplicate = compile("type Word = i32; type Word = u32; fn main() -> i32 { return 0; }").unwrap_err();
        assert!(duplicate.contains("duplicate type alias 'Word'"));
        let conflict = compile("struct Point { x: i32 } type Point = i32; fn main() -> i32 { return 0; }").unwrap_err();
        assert!(conflict.contains("conflicts with an existing type declaration"));
    }

    #[test]
    fn emits_global_scalar_and_string_constants() {
        let source = r#"const ANSWER: i32 = 42; const GREETING: str = "hello"; const ENABLED: bool = true; fn main() -> i32 { print(ANSWER); print(GREETING); print(ENABLED); return 0; }"#;
        let ir = compile(source).unwrap();
        assert!(ir.contains("@ANSWER = constant i32 42"));
        assert!(ir.contains("@ENABLED = constant i1 true"));
        assert!(ir.contains("@GREETING = private unnamed_addr constant [6 x i8]"));
        assert!(ir.contains("getelementptr inbounds ([6 x i8], ptr @GREETING"));
    }

    #[test]
    fn evaluates_integer_float_and_boolean_global_constant_expressions() {
        let source = "const PAGE_SIZE: i32 = 4096; const PAGE_SHIFT: i32 = 1 << 12; const PAGE_MASK: u32 = 65535 & 4095; const WIDTH: i32 = 1 + 1 * 3; const NEGATIVE: i32 = -(2 + 3); const HALF: f64 = 10.0 / 2.0; const VALID: bool = (4 & 1) == 0 && !false; fn main() -> i32 { print(PAGE_SIZE); print(PAGE_SHIFT); print(PAGE_MASK); print(WIDTH); print(NEGATIVE); print(HALF); print(VALID); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("@PAGE_SIZE = constant i32 4096"));
        assert!(ir.contains("@PAGE_SHIFT = constant i32 4096"));
        assert!(ir.contains("@PAGE_MASK = constant i32 4095"));
        assert!(ir.contains("@WIDTH = constant i32 4"));
        assert!(ir.contains("@NEGATIVE = constant i32 -5"));
        assert!(ir.contains("@HALF = constant double 5.00000000000000000e0"));
        assert!(ir.contains("@VALID = constant i1 true"));
    }

    #[test]
    fn rejects_overflowing_and_invalid_constant_expressions() {
        let overflow = compile("const VALUE: i8 = 127 + 1; fn main() -> i32 { return 0; }").unwrap_err();
        assert!(overflow.contains("requires a literal compatible with type i8"));
        let divide_by_zero = compile("const VALUE: i32 = 10 / 0; fn main() -> i32 { return 0; }").unwrap_err();
        assert!(divide_by_zero.contains("division by zero in integer constant expression"));
        let float_overflow = compile("const VALUE: f64 = 1.0 / 0.0; fn main() -> i32 { return 0; }").unwrap_err();
        assert!(float_overflow.contains("floating-point constant expression is outside the finite range"));
    }

    #[test]
    fn evaluates_constants_using_earlier_constant_values() {
        let source = "const BASE: i32 = 40; const ANSWER: i32 = BASE + 2; const VALID: bool = ANSWER == 42; const RATE: f64 = 5.0 * 2.0; fn main() -> i32 { print(ANSWER); print(VALID); print(RATE); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("@BASE = constant i32 40"));
        assert!(ir.contains("@ANSWER = constant i32 42"));
        assert!(ir.contains("@VALID = constant i1 true"));
        assert!(ir.contains("@RATE = constant double 1.00000000000000000e1"));
    }

    #[test]
    fn rejects_forward_references_in_global_constants() {
        let error = compile("const ANSWER: i32 = BASE + 2; const BASE: i32 = 40; fn main() -> i32 { return 0; }").unwrap_err();
        assert!(error.contains("requires a literal compatible with type i32"));
    }

    #[test]
    fn lowers_mutable_and_immutable_static_variables() {
        let source = r#"static mut COUNTER: i32 = 40; static LIMIT: u32 = 42; static MESSAGE: str = "ready"; fn main() -> i32 { COUNTER = COUNTER + 2; print(COUNTER); print(LIMIT); print(MESSAGE); let p: ptr = &COUNTER; store_i32(p, 43); print(COUNTER); return 0; }"#;
        let ir = compile(source).unwrap();
        assert!(ir.contains("@COUNTER = global i32 40"));
        assert!(ir.contains("@LIMIT = constant i32 42"));
        assert!(ir.contains("@.ramlang.static.MESSAGE = private unnamed_addr constant [6 x i8]"));
        assert!(ir.contains("@MESSAGE = constant ptr getelementptr inbounds ([6 x i8], ptr @.ramlang.static.MESSAGE"));
        assert!(ir.contains("store i32"));
        assert!(ir.contains("load i32, ptr @COUNTER"));
    }

    #[test]
    fn lowers_mutable_static_array_literals_to_global_storage() {
        let source = "static mut VALUES: [i32; 3] = [10, 20, 30]; fn main() -> i32 { print(VALUES[1]); VALUES[1] = 42; print(VALUES[1]); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("@.ramlang.static.storage.VALUES = global [3 x i32] [i32 10, i32 20, i32 30]"));
        assert!(ir.contains("@VALUES = constant ptr getelementptr inbounds ([3 x i32], ptr @.ramlang.static.storage.VALUES, i64 0, i64 0)"));
        assert!(ir.contains("getelementptr inbounds i32"));
        assert!(ir.contains("store i32 42"));
    }

    #[test]
    fn lowers_immutable_static_array_literals_to_read_only_global_storage() {
        let source = "static TABLE: [i32; 3] = [10, 20, 30]; fn main() -> i32 { print(TABLE[1]); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("@.ramlang.static.storage.TABLE = constant [3 x i32] [i32 10, i32 20, i32 30]"));
        assert!(ir.contains("@TABLE = constant ptr getelementptr inbounds ([3 x i32], ptr @.ramlang.static.storage.TABLE, i64 0, i64 0)"));
        assert!(ir.contains("load i32, ptr"));
    }

    #[test]
    fn rejects_direct_mutation_of_immutable_static_array_elements() {
        let error = compile("static TABLE: [i32; 2] = [10, 20]; fn main() -> i32 { TABLE[0] = 7; return 0; }").unwrap_err();
        assert!(error.contains("cannot assign to an element of immutable static array 'TABLE'"));
    }

    #[test]
    fn emits_extern_static_global_declarations() {
        let source = "extern static mut C_COUNTER: i32; extern static C_LIMIT: u32; fn main() -> i32 { C_COUNTER = C_COUNTER + 2; print(C_COUNTER); print(C_LIMIT); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("@C_COUNTER = external global i32"));
        assert!(ir.contains("@C_LIMIT = external constant i32"));
        assert!(ir.contains("load i32, ptr @C_COUNTER"));
        assert!(ir.contains("store i32"));
    }

    #[test]
    fn rejects_aggregate_extern_static_declarations() {
        let error = parse_source("struct Point { x: i32 } extern static point: Point; fn main() -> i32 { return 0; }").unwrap_err();
        assert!(error.contains("extern static currently supports scalar and raw-pointer types only"));
    }

    #[test]
    fn lowers_static_pointer_address_initializers() {
        let source = "static mut UART_BASE: ptr = 1073741824 as ptr; fn main() -> i32 { let address: ptr = UART_BASE; return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("@UART_BASE = global ptr inttoptr (i64 1073741824 to ptr)"));
        assert!(ir.contains("load ptr, ptr @UART_BASE"));
    }

    #[test]
    fn rejects_assignment_to_immutable_static_variables() {
        let error = compile("static VALUE: i32 = 1; fn main() -> i32 { VALUE = 2; return 0; }").unwrap_err();
        assert!(error.contains("cannot assign to immutable variable 'VALUE'"));
    }

    #[test]
    fn rejects_duplicate_or_conflicting_global_symbols() {
        let duplicate = compile("static mut VALUE: i32 = 1; const VALUE: i32 = 2; fn main() -> i32 { return 0; }").unwrap_err();
        assert!(duplicate.contains("duplicate global symbol 'VALUE'"));
        let function_collision = compile("static mut run: i32 = 1; fn run() -> i32 { return 0; } fn main() -> i32 { return 0; }").unwrap_err();
        assert!(function_collision.contains("static variable 'run' conflicts with a function"));
    }

    #[test]
    fn lowers_assertions_and_optimizer_assumptions() {
        let source = "fn main() -> i32 { let value: i32 = 42; assert(value == 42); assume(value > 0); print(value); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("assert.ok"));
        assert!(ir.contains("assert.trap"));
        assert!(ir.contains("call void @llvm.trap()"));
        assert!(ir.contains("call void @llvm.assume(i1"));
    }

    #[test]
    fn lowers_panic_as_a_non_returning_trap() {
        let source = r#"fn main() -> i32 { panic("fatal error"); return 0; }"#;
        let ir = compile(source).unwrap();
        assert!(ir.contains("@.fmt.str"));
        assert!(ir.contains("@.str.main.0"));
        assert!(ir.contains(r#"\66\61\74"#));
        assert!(ir.contains("call void @llvm.trap()"));
        assert!(ir.contains("unreachable"));
        assert!(!ir.contains("ret i32 0"));
    }

    #[test]
    fn rejects_invalid_panic_arguments() {
        let error = compile("fn main() -> i32 { panic(42); return 0; }").unwrap_err();
        assert!(error.contains("panic message"));
    }

    #[test]
    fn accepts_panic_as_a_non_returning_path_in_nonvoid_functions() {
        let source = r#"fn fail() -> i32 { panic("cannot continue"); } fn main() -> i32 { fail(); return 0; }"#;
        let ir = compile(source).unwrap();
        assert!(ir.contains("define i32 @fail()"));
        assert!(ir.contains("call void @llvm.trap()"));
        assert!(ir.contains("unreachable"));
    }

    #[test]
    fn rejects_invalid_assert_and_assume_arguments() {
        let assert_error = compile("fn main() -> i32 { assert(1); return 0; }").unwrap_err();
        assert!(assert_error.contains("assert condition"));
        let assume_count = compile("fn main() -> i32 { assume(true, false); return 0; }").unwrap_err();
        assert!(assume_count.contains("assume expects exactly one bool argument"));
    }

    #[test]
    fn rejects_invalid_and_duplicate_global_constants() {
        let range_error = compile("const VALUE: i8 = 300; fn main() -> i32 { return 0; }").unwrap_err();
        assert!(range_error.contains("requires a literal compatible with type i8"));
        let duplicate = compile("const VALUE: i32 = 1; const VALUE: i32 = 2; fn main() -> i32 { return 0; }").unwrap_err();
        assert!(duplicate.contains("duplicate global constant 'VALUE'"));
        let collision = compile("const value: i32 = 1; fn value() -> i32 { return 0; } fn main() -> i32 { return 0; }").unwrap_err();
        assert!(collision.contains("conflicts with a function"));
    }

    #[test]
    fn selects_pointer_width_from_target_triple() {
        assert_eq!(target_pointer_width(Some("i686-unknown-linux-gnu")).unwrap(), 32);
        assert_eq!(target_pointer_width(Some("wasm32-unknown-unknown")).unwrap(), 32);
        assert_eq!(target_pointer_width(Some("x86_64-unknown-linux-gnu")).unwrap(), 64);
        assert_eq!(target_pointer_width(Some("aarch64-unknown-linux-gnu")).unwrap(), 64);
        assert_eq!(target_pointer_width(Some("x86_64-unknown-linux-gnux32")).unwrap(), 32);
        assert!(target_pointer_width(Some("unknown-unknown-unknown")).unwrap_err().contains("cannot determine pointer width"));
    }

    #[test]
    fn lowers_size_t_based_apis_for_32bit_targets() {
        let source = "fn main() -> i32 { let p: ptr = calloc(2, size_of(i32)); let text_length: i64 = strlen(\"hello\"); memset(p, 0, text_length); memcpy(p, p, text_length); let order: i32 = memcmp(p, p, text_length); let found: ptr = memchr(p, 0, text_length); let q: ptr = realloc(p, text_length); free(q); return order; }";
        let program = parse_source(source).unwrap();
        let ir = compile_to_llvm_for_target(&program, 32, Some("i386-unknown-linux-gnu")).unwrap();
        assert!(ir.contains("target triple = \"i386-unknown-linux-gnu\""));
        assert!(ir.contains("declare ptr @malloc(i32)"));
        assert!(ir.contains("declare ptr @calloc(i32, i32)"));
        assert!(ir.contains("declare ptr @realloc(ptr, i32)"));
        assert!(ir.contains("declare ptr @memcpy(ptr, ptr, i32)"));
        assert!(ir.contains("declare ptr @memset(ptr, i32, i32)"));
        assert!(ir.contains("declare i32 @strlen(ptr)"));
        assert!(ir.contains("declare i32 @strncmp(ptr, ptr, i32)"));
        assert!(ir.contains("declare ptr @memchr(ptr, i32, i32)"));
        assert!(ir.contains("call ptr @calloc(i32"));
        assert!(ir.contains("call i32 @strlen(ptr"));
        assert!(ir.contains("zext i32"));
        assert!(ir.contains("trunc i64"));
        assert!(ir.contains("size_t.trap"));
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
    fn parses_target_cpu_features_and_sysroot() {
        let options = parse_args_from([
            "example.ram".to_owned(),
            "--target".to_owned(),
            "x86_64-unknown-linux-gnu".to_owned(),
            "--target-cpu".to_owned(),
            "x86-64".to_owned(),
            "--target-feature".to_owned(),
            "+sse2".to_owned(),
            "--target-feature".to_owned(),
            "-avx".to_owned(),
            "--sysroot".to_owned(),
            "/opt/sysroot".to_owned(),
        ]).unwrap();
        assert_eq!(options.target_cpu.as_deref(), Some("x86-64"));
        assert_eq!(options.target_features, vec!["+sse2", "-avx"]);
        assert_eq!(options.sysroot, Some(PathBuf::from("/opt/sysroot")));
    }

    #[test]
    fn rejects_invalid_target_cpu_and_features() {
        let cpu = parse_args_from([
            "example.ram".to_owned(), "--target-cpu".to_owned(), "-invalid".to_owned()
        ]).unwrap_err();
        assert!(cpu.contains("invalid target CPU"));

        let feature = parse_args_from([
            "example.ram".to_owned(), "--target-feature".to_owned(), "avx2".to_owned()
        ]).unwrap_err();
        assert!(feature.contains("expected +feature or -feature"));

        let malformed = parse_args_from([
            "example.ram".to_owned(), "--target-feature".to_owned(), "+avx; -sse".to_owned()
        ]).unwrap_err();
        assert!(malformed.contains("invalid target feature"));
    }

    #[test]
    fn rejects_backend_target_options_for_check_and_ir_only_modes() {
        for mode in ["--check", "--emit-llvm"] {
            let error = parse_args_from([
                "example.ram".to_owned(),
                mode.to_owned(),
                "--target-cpu".to_owned(),
                "x86-64".to_owned(),
            ]).unwrap_err();
            assert!(error.contains("target CPU, target features, and sysroot require backend code generation"));
        }

        let sysroot = parse_args_from([
            "example.ram".to_owned(),
            "--emit-llvm".to_owned(),
            "--sysroot".to_owned(),
            "/".to_owned(),
        ]).unwrap_err();
        assert!(sysroot.contains("target CPU, target features, and sysroot require backend code generation"));
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
    fn resolves_relative_imports_transitively_and_deduplicates_files() {
        let root = env::temp_dir().join(format!("ramlang-import-test-{}", process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("math")).unwrap();
        fs::write(
            root.join("main.ram"),
            "import \"math/add.ram\"; import \"math/add.ram\"; fn main() -> i32 { return add(20, 22) - 42; }",
        ).unwrap();
        fs::write(
            root.join("math/add.ram"),
            "import \"../constants.ram\"; fn add(a: i32, b: i32) -> i32 { return a + b + OFFSET; }",
        ).unwrap();
        fs::write(root.join("constants.ram"), "const OFFSET: i32 = 0;").unwrap();

        let mut program = Program::default();
        let mut visited = HashSet::new();
        let loaded = load_source_recursive(&root.join("main.ram"), &mut visited).unwrap();
        assert_eq!(loaded.len(), 3);
        for part in loaded {
            merge_program(&mut program, part).unwrap();
        }
        let ir = compile_to_llvm(&program).unwrap();
        assert!(ir.contains("define i32 @add(i32"));
        assert!(ir.contains("@OFFSET = constant i32 0"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn import_cycles_do_not_parse_the_same_file_twice() {
        let root = env::temp_dir().join(format!("ramlang-import-cycle-test-{}", process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("main.ram"), "import \"helper.ram\"; fn main() -> i32 { return helper() - 42; }").unwrap();
        fs::write(root.join("helper.ram"), "import \"main.ram\"; fn helper() -> i32 { return 42; }").unwrap();

        let mut program = Program::default();
        let mut visited = HashSet::new();
        let loaded = load_source_recursive(&root.join("main.ram"), &mut visited).unwrap();
        assert_eq!(loaded.len(), 2);
        for part in loaded {
            merge_program(&mut program, part).unwrap();
        }
        assert!(compile_to_llvm(&program).is_ok());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reports_missing_import_paths() {
        let root = env::temp_dir().join(format!("ramlang-missing-import-test-{}", process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("main.ram"), "import \"missing.ram\"; fn main() -> i32 { return 0; }").unwrap();
        let mut visited = HashSet::new();
        let error = load_source_recursive(&root.join("main.ram"), &mut visited).unwrap_err();
        assert!(error.contains("missing.ram"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn generates_c_header_for_functions_globals_and_enums() {
        let source = "enum Mode { Idle = 0, Active = 2 } static mut counter: i32 = 0; static limit: u32 = 100; const ratio: f32 = 0.5; extern static C_LIMIT: u64; extern fn printf(format: str, ...) -> i32; fn add(left: i32, right: i32) -> i32 { return left + right; } fn convert(value: f32) -> f64 { return value as f64; } fn main() -> i32 { return 0; }";
        let program = parse_source(source).unwrap();
        let header = generate_c_header(&program).unwrap();
        assert!(header.contains("#include <stdbool.h>"));
        assert!(header.contains("#include <stdint.h>"));
        assert!(header.contains("#define RAMLANG_MODE_IDLE (0)"));
        assert!(header.contains("#define RAMLANG_MODE_ACTIVE (2)"));
        assert!(header.contains("extern int32_t add(int32_t left, int32_t right);"));
        assert!(header.contains("extern double convert(float value);"));
        assert!(header.contains("extern int32_t printf(const char * format, ...);"));
        assert!(header.contains("extern int32_t counter;"));
        assert!(header.contains("extern const uint32_t limit;"));
        assert!(header.contains("extern const float ratio;"));
        assert!(header.contains("extern const uint64_t C_LIMIT;"));
        assert!(!header.contains("main("));
    }

    #[test]
    fn rejects_c_header_names_that_are_c_keywords() {
        let program = parse_source("fn switch(value: i32) -> i32 { return value; } fn main() -> i32 { return 0; }").unwrap();
        let error = generate_c_header(&program).unwrap_err();
        assert!(error.contains("function name 'switch' is not a valid C identifier"));
    }

    #[test]
    fn parses_object_assembly_bitcode_and_header_emission_modes() {
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
        assert!(!object.emit_bitcode);
        assert_eq!(object.output, Some(PathBuf::from("example.o")));
        assert_eq!(object.opt_level, "0");

        let assembly = parse_args_from([
            "example.ram".to_owned(),
            "--emit-asm".to_owned(),
        ]).unwrap();
        assert!(assembly.emit_assembly);
        assert_eq!(assembly.sources[0], PathBuf::from("example.ram"));

        let bitcode = parse_args_from([
            "example.ram".to_owned(),
            "--emit-bc".to_owned(),
            "--target".to_owned(),
            "x86_64-unknown-linux-gnu".to_owned(),
            "--opt-level".to_owned(),
            "3".to_owned(),
        ]).unwrap();
        assert!(bitcode.emit_bitcode);
        assert_eq!(bitcode.opt_level, "3");

        let header = parse_args_from([
            "api.ram".to_owned(),
            "--emit-header".to_owned(),
            "api.h".to_owned(),
        ]).unwrap();
        assert!(header.emit_header);
        assert_eq!(header.output, Some(PathBuf::from("api.h")));
    }

    #[test]
    fn rejects_conflicting_emit_modes() {
        let error = parse_args_from([
            "example.ram".to_owned(),
            "--emit-obj".to_owned(),
            "--emit-bc".to_owned(),
        ]).unwrap_err();
        assert!(error.contains("mutually exclusive"));
    }

    #[test]
    fn rejects_linking_options_in_emit_only_modes() {
        let object_error = parse_args_from([
            "example.ram".to_owned(),
            "--emit-obj".to_owned(),
            "--link-lib".to_owned(),
            "m".to_owned(),
        ]).unwrap_err();
        assert!(object_error.contains("linker options require native linking"));

        let bitcode_error = parse_args_from([
            "example.ram".to_owned(),
            "--emit-bc".to_owned(),
            "--link-lib".to_owned(),
            "m".to_owned(),
        ]).unwrap_err();
        assert!(bitcode_error.contains("linker options require native linking"));
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
    fn allows_target_specific_llvm_ir_emission() {
        let options = parse_args_from([
            "example.ram".to_owned(),
            "--emit-llvm".to_owned(),
            "example.ll".to_owned(),
            "--target".to_owned(),
            "i386-unknown-linux-gnu".to_owned(),
        ]).unwrap();
        assert!(options.emit_llvm);
        assert_eq!(options.target.as_deref(), Some("i386-unknown-linux-gnu"));
    }

    #[test]
    fn rejects_target_and_explicit_optimization_for_ir_only_modes() {
        let error = parse_args_from([
            "example.ram".to_owned(),
            "--check".to_owned(),
            "--opt-level".to_owned(),
            "2".to_owned(),
        ]).unwrap_err();
        assert!(error.contains("optimization options require native code generation"));
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
    fn lowers_bulk_memory_copy_move_fill_and_compare() {
        let source = "fn main() -> i32 { let src: ptr = alloc(8); let dst: ptr = alloc(8); memset(src, 0, 8); store_i32(src, 42); memcpy(dst, src, 8); print(load_i32(dst)); print(memcmp(src, dst, 8)); store_i32(dst, 7); let order: i32 = memcmp(src, dst, 4); if (order > 0) { print(1); } else { print(0); } memmove(dst, src, 4); print(load_i32(dst)); free(src); free(dst); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("declare ptr @memcpy(ptr, ptr, i64)"));
        assert!(ir.contains("declare ptr @memmove(ptr, ptr, i64)"));
        assert!(ir.contains("declare ptr @memset(ptr, i32, i64)"));
        assert!(ir.contains("declare i32 @memcmp(ptr, ptr, i64)"));
        assert!(ir.contains("call ptr @memcpy(ptr"));
        assert!(ir.contains("call ptr @memmove(ptr"));
        assert!(ir.contains("call ptr @memset(ptr"));
        assert!(ir.contains("call i32 @memcmp(ptr"));
    }

    #[test]
    fn lowers_calloc_and_realloc() {
        let source = "fn main() -> i32 { let p: ptr = calloc(4, size_of(i32)); store_i32(p, 42); let q: ptr = realloc(p, 32); print(load_i32(q)); free(q); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("declare ptr @calloc(i64, i64)"));
        assert!(ir.contains("declare ptr @realloc(ptr, i64)"));
        assert!(ir.contains("call ptr @calloc(i64 4, i64 4)"));
        assert!(ir.contains("call ptr @realloc(ptr"));
    }

    #[test]
    fn lowers_c_string_and_byte_search_builtins() {
        let source = "fn main() -> i32 { print(strlen(\"RamLang\")); print(strcmp(\"RamLang\", \"RamLang\")); print(strncmp(\"RamLang\", \"Ram\", 3)); let found: ptr = memchr(\"RamLang\", 76, 7); print(load_u8(found)); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("declare i64 @strlen(ptr)"));
        assert!(ir.contains("declare i32 @strcmp(ptr, ptr)"));
        assert!(ir.contains("declare i32 @strncmp(ptr, ptr, i64)"));
        assert!(ir.contains("declare ptr @memchr(ptr, i32, i64)"));
        assert!(ir.contains("call i64 @strlen(ptr"));
        assert!(ir.contains("call i32 @strcmp(ptr"));
        assert!(ir.contains("call i32 @strncmp(ptr"));
        assert!(ir.contains("call ptr @memchr(ptr"));
    }

    #[test]
    fn rejects_invalid_string_and_memchr_arguments() {
        let length_error = compile("fn main() -> i32 { let n: i64 = strlen(1); return 0; }").unwrap_err();
        assert!(length_error.contains("strlen argument requires a pointer-like value"));
        let compare_error = compile("fn main() -> i32 { let n: i32 = strcmp(\"x\"); return 0; }").unwrap_err();
        assert!(compare_error.contains("strcmp expects 2 arguments"));
        let search_error = compile("fn main() -> i32 { let p: ptr = memchr(\"x\", 256.0, 1); return 0; }").unwrap_err();
        assert!(search_error.contains("memchr byte value"));
    }

    #[test]
    fn rejects_invalid_calloc_and_realloc_arguments() {
        let calloc_error = compile("fn main() -> i32 { let p: ptr = calloc(4); return 0; }").unwrap_err();
        assert!(calloc_error.contains("calloc expects an element count and element size"));
        let realloc_error = compile("fn main() -> i32 { let p: ptr = realloc(null); return 0; }").unwrap_err();
        assert!(realloc_error.contains("realloc expects an existing pointer and a new byte count"));
        let type_error = compile("fn main() -> i32 { let p: ptr = calloc(true, 4); return 0; }").unwrap_err();
        assert!(type_error.contains("calloc element count"));
    }

    #[test]
    fn rejects_invalid_bulk_memory_arguments() {
        let count_error = compile("fn main() -> i32 { let p: ptr = alloc(8); memcpy(p, p); free(p); return 0; }").unwrap_err();
        assert!(count_error.contains("expects destination, source, and byte count"));
        let pointer_error = compile("fn main() -> i32 { memset(1, 0, 4); return 0; }").unwrap_err();
        assert!(pointer_error.contains("memset destination requires a pointer-like value"));
    }

    #[test]
    fn lowers_volatile_loads_and_stores() {
        let source = "fn main() -> i32 { let mut value: i32 = 0; let address: ptr = &value; volatile_store_i32(address, 12); let result: i32 = volatile_load_i32(address); print(result); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("store volatile i32 12, ptr"));
        assert!(ir.contains("load volatile i32, ptr"));
    }

    #[test]
    fn lowers_unaligned_memory_accesses_with_alignment_one() {
        let source = "fn main() -> i32 { let p: ptr = alloc(8); let q: ptr = ptr_offset(p, 1); store_unaligned_i32(q, 42); let value: i32 = load_unaligned_i32(q); volatile_store_unaligned_i16(q, 7); let other: i16 = volatile_load_unaligned_i16(q); print(value); print(other); free(p); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("store i32 42, ptr"));
        assert!(ir.contains("load i32, ptr"));
        assert!(ir.contains("store volatile i16 7, ptr"));
        assert!(ir.contains("load volatile i16, ptr"));
        assert!(ir.contains(", align 1"));
        assert!(ir.contains("store i32 42, ptr") && ir.contains("align 1"));
    }

    #[test]
    fn lowers_memory_order_fences() {
        let source = "fn main() -> i32 { fence_acquire(); fence_release(); fence_acq_rel(); fence_seq_cst(); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("fence acquire"));
        assert!(ir.contains("fence release"));
        assert!(ir.contains("fence acq_rel"));
        assert!(ir.contains("fence seq_cst"));
    }

    #[test]
    fn lowers_inline_assembly_with_literal_template_and_constraints() {
        let source = r#"fn main() -> i32 { let value: i32 = asm_i32("mov $1, $0", "=r,r", 42); asm_void("nop", ""); print(value); return 0; }"#;
        let ir = compile(source).unwrap();
        assert!(ir.contains(r#"call i32 asm sideeffect "\6D\6F\76\20\24\31\2C\20\24\30", "\3D\72\2C\72"(i32 42)"#));
        assert!(ir.contains(r#"call void asm sideeffect "\6E\6F\70", ""()"#));
    }

    #[test]
    fn rejects_invalid_inline_assembly_template_and_constraints() {
        let missing = compile("fn main() -> i32 { let x: i32 = asm_i32(42, \"=r\", 1); return x; }").unwrap_err();
        assert!(missing.contains("assembly template must be a string literal"));
        let bad_constraints = compile("fn main() -> i32 { let x: i32 = asm_i32(\"nop\", 0, 1); return x; }").unwrap_err();
        assert!(bad_constraints.contains("constraints must be a string literal"));
        let too_few = compile("fn main() -> i32 { let x: i32 = asm_i32(\"nop\"); return x; }").unwrap_err();
        assert!(too_few.contains("expects an assembly string and a constraint string"));
    }

    #[test]
    fn lowers_atomic_load_store_and_read_modify_write() {
        let source = "fn main() -> i32 { let mut value: i32 = 0; let address: ptr = &value; atomic_store_i32_release(address, 10); print(atomic_load_i32_acquire(address)); print(atomic_fetch_add_i32_acq_rel(address, 5)); print(atomic_fetch_sub_i32_relaxed(address, 1)); print(atomic_exchange_i32_seq_cst(address, 20)); print(atomic_fetch_and_i32(address, 7)); print(atomic_fetch_or_i32(address, 8)); print(atomic_fetch_xor_i32(address, 1)); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("store atomic i32 10, ptr"));
        assert!(ir.contains(" acquire, align 4"));
        assert!(ir.contains(" atomicrmw add ptr"));
        assert!(ir.contains(" acq_rel"));
        assert!(ir.contains(" atomicrmw sub ptr"));
        assert!(ir.contains(" monotonic"));
        assert!(ir.contains(" atomicrmw xchg ptr"));
        assert!(ir.contains(" atomicrmw and ptr"));
        assert!(ir.contains(" atomicrmw or ptr"));
        assert!(ir.contains(" atomicrmw xor ptr"));
    }

    #[test]
    fn lowers_compare_exchange_with_expected_value_writeback() {
        let source = "fn main() -> i32 { let mut value: i32 = 10; let mut expected: i32 = 10; let address: ptr = &value; let expected_address: ptr = &expected; let success: bool = atomic_compare_exchange_i32_acq_rel(address, expected_address, 20); print(success); print(value); expected = 15; let failed: bool = atomic_compare_exchange_i32_release(address, expected_address, 30); print(failed); print(expected); print(value); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("cmpxchg ptr"));
        assert!(ir.contains(" acq_rel acquire, align 4"));
        assert!(ir.contains(" release monotonic, align 4"));
        assert!(ir.contains("extractvalue { i32, i1 }"));
        assert!(ir.contains("store i32"));
    }

    #[test]
    fn lowers_atomic_nand_and_signed_unsigned_min_max() {
        let source = "fn main() -> i32 { let mut signed: i32 = -1; let mut unsigned: u32 = 0; let signed_address: ptr = &signed; let unsigned_address: ptr = &unsigned; print(atomic_fetch_nand_i32(signed_address, 255)); print(atomic_fetch_max_i32(signed_address, 0)); print(atomic_fetch_min_i32(signed_address, 1)); print(atomic_fetch_umax_u32(unsigned_address, 5)); print(atomic_fetch_umin_u32(unsigned_address, 3)); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("atomicrmw nand ptr"));
        assert!(ir.contains("atomicrmw max ptr"));
        assert!(ir.contains("atomicrmw min ptr"));
        assert!(ir.contains("atomicrmw umax ptr"));
        assert!(ir.contains("atomicrmw umin ptr"));
    }

    #[test]
    fn rejects_invalid_atomic_memory_orderings_and_types() {
        let load_order = compile("fn main() -> i32 { let mut value: i32 = 0; let p: ptr = &value; let x: i32 = atomic_load_i32_release(p); return x; }").unwrap_err();
        assert!(load_order.contains("atomic loads support relaxed, acquire, or seq_cst ordering"));
        let store_order = compile("fn main() -> i32 { let mut value: i32 = 0; let p: ptr = &value; atomic_store_i32_acquire(p, 1); return 0; }").unwrap_err();
        assert!(store_order.contains("atomic stores support relaxed, release, or seq_cst ordering"));
        let type_error = compile("fn main() -> i32 { let mut value: i32 = 0; let p: ptr = &value; atomic_store_i32(p, true); return 0; }").unwrap_err();
        assert!(type_error.contains("atomic_store value"));
        let count_error = compile("fn main() -> i32 { let mut value: i32 = 0; let p: ptr = &value; let ok: bool = atomic_compare_exchange_i32(p, p); return 0; }").unwrap_err();
        assert!(count_error.contains("atomic_compare_exchange_* expects 3 argument(s)"));
    }

    #[test]
    fn lowers_integer_bit_manipulation_intrinsics() {
        let source = "fn main() -> i32 { print(ctpop_32(255)); print(ctlz_32(1)); print(cttz_32(8)); print(bswap_32(16909060)); print(bitreverse_32(1)); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("call i32 @llvm.ctpop.i32(i32 255)"));
        assert!(ir.contains("call i32 @llvm.ctlz.i32(i32 1, i1 false)"));
        assert!(ir.contains("call i32 @llvm.cttz.i32(i32 8, i1 false)"));
        assert!(ir.contains("call i32 @llvm.bswap.i32(i32 16909060)"));
        assert!(ir.contains("call i32 @llvm.bitreverse.i32(i32 1)"));
    }

    #[test]
    fn lowers_checked_signed_and_unsigned_arithmetic() {
        let source = "fn main() -> i32 { let mut signed_result: i32 = 0; let signed_out: ptr = &signed_result; let signed_overflow: bool = sadd_overflow_i32(2147483647, 1, signed_out); print(signed_result); print(signed_overflow); let mut unsigned_result: u16 = 0; let unsigned_out: ptr = &unsigned_result; let unsigned_overflow: bool = uadd_overflow_u16(65535, 1, unsigned_out); print(unsigned_result); print(unsigned_overflow); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("call { i32, i1 } @llvm.sadd.with.overflow.i32(i32 2147483647, i32 1)"));
        assert!(ir.contains("call { i16, i1 } @llvm.uadd.with.overflow.i16(i16 65535, i16 1)"));
        assert!(ir.contains("extractvalue { i32, i1 }"));
        assert!(ir.contains("extractvalue { i16, i1 }"));
        assert!(ir.contains("store i32"));
        assert!(ir.contains("store i16"));
    }

    #[test]
    fn lowers_checked_arithmetic_for_all_integer_widths_and_operations() {
        let source = "fn main() -> i32 { let mut a8: i8 = 0; let p8: ptr = &a8; let x1: bool = sadd_overflow_i8(127, 1, p8); let x2: bool = ssub_overflow_i8(-128, 1, p8); let x3: bool = smul_overflow_i8(100, 2, p8); let mut a16: i16 = 0; let p16: ptr = &a16; let y1: bool = sadd_overflow_i16(32767, 1, p16); let y2: bool = ssub_overflow_i16(-32768, 1, p16); let y3: bool = smul_overflow_i16(20000, 2, p16); let mut a32: i32 = 0; let p32: ptr = &a32; let z1: bool = sadd_overflow_i32(2147483647, 1, p32); let z2: bool = ssub_overflow_i32(-2147483648, 1, p32); let z3: bool = smul_overflow_i32(2000000000, 2, p32); let mut a64: i64 = 0; let p64: ptr = &a64; let w1: bool = sadd_overflow_i64(9223372036854775807, 1, p64); let w2: bool = ssub_overflow_i64(-9223372036854775808, 1, p64); let w3: bool = smul_overflow_i64(4000000000000000000, 3, p64); let mut b8: u8 = 0; let q8: ptr = &b8; let u1: bool = uadd_overflow_u8(255, 1, q8); let u2: bool = usub_overflow_u8(0, 1, q8); let u3: bool = umul_overflow_u8(200, 2, q8); let mut b16: u16 = 0; let q16: ptr = &b16; let v1: bool = uadd_overflow_u16(65535, 1, q16); let v2: bool = usub_overflow_u16(0, 1, q16); let v3: bool = umul_overflow_u16(50000, 2, q16); let mut b32: u32 = 0; let q32: ptr = &b32; let r1: bool = uadd_overflow_u32(4294967295, 1, q32); let r2: bool = usub_overflow_u32(0, 1, q32); let r3: bool = umul_overflow_u32(3000000000, 2, q32); let mut b64: u64 = 0; let q64: ptr = &b64; let s1: bool = uadd_overflow_u64(18446744073709551615, 1, q64); let s2: bool = usub_overflow_u64(0, 1, q64); let s3: bool = umul_overflow_u64(10000000000000000000, 2, q64); return 0; }";
        let ir = compile(source).unwrap();
        for llvm_type in ["i8", "i16", "i32", "i64"] {
            for operation in ["sadd", "ssub", "smul", "uadd", "usub", "umul"] {
                assert!(ir.contains(&format!("@llvm.{operation}.with.overflow.{llvm_type}")),
                    "missing checked arithmetic intrinsic {operation}.{llvm_type}");
            }
        }
    }

    #[test]
    fn lowers_saturating_arithmetic_for_all_integer_widths() {
        let source = "fn main() -> i32 { let a8: i8 = sadd_sat_i8(127, 1); let b8: i8 = ssub_sat_i8(-128, 1); let c8: u8 = uadd_sat_u8(255, 1); let d8: u8 = usub_sat_u8(0, 1); let a16: i16 = sadd_sat_i16(32767, 1); let b16: i16 = ssub_sat_i16(-32768, 1); let c16: u16 = uadd_sat_u16(65535, 1); let d16: u16 = usub_sat_u16(0, 1); let a32: i32 = sadd_sat_i32(2147483647, 1); let b32: i32 = ssub_sat_i32(-2147483648, 1); let c32: u32 = uadd_sat_u32(4294967295, 1); let d32: u32 = usub_sat_u32(0, 1); let a64: i64 = sadd_sat_i64(9223372036854775807, 1); let b64: i64 = ssub_sat_i64(-9223372036854775808, 1); let c64: u64 = uadd_sat_u64(18446744073709551615, 1); let d64: u64 = usub_sat_u64(0, 1); return 0; }";
        let ir = compile(source).unwrap();
        for llvm_type in ["i8", "i16", "i32", "i64"] {
            for operation in ["sadd.sat", "ssub.sat", "uadd.sat", "usub.sat"] {
                assert!(ir.contains(&format!("@llvm.{operation}.{llvm_type}")),
                    "missing saturating arithmetic intrinsic {operation}.{llvm_type}");
            }
        }
    }

    #[test]
    fn lowers_signed_and_unsigned_saturating_add_sub() {
        let source = "fn main() -> i32 { let a: i32 = sadd_sat_i32(2147483647, 1); let b: i32 = ssub_sat_i32(-2147483648, 1); let c: u16 = uadd_sat_u16(65535, 1); let d: u16 = usub_sat_u16(0, 1); print(a); print(b); print(c); print(d); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("call i32 @llvm.sadd.sat.i32(i32 2147483647, i32 1)"));
        assert!(ir.contains("call i32 @llvm.ssub.sat.i32(i32 -2147483648, i32 1)"));
        assert!(ir.contains("call i16 @llvm.uadd.sat.i16(i16 65535, i16 1)"));
        assert!(ir.contains("call i16 @llvm.usub.sat.i16(i16 0, i16 1)"));
    }

    #[test]
    fn rejects_invalid_saturating_arithmetic_arguments() {
        let count = compile("fn main() -> i32 { let x: i32 = sadd_sat_i32(1); return x; }").unwrap_err();
        assert!(count.contains("expects exactly two integer arguments"));
        let signedness = compile("fn main() -> i32 { let x: u32 = sadd_sat_u32(1 as u32, 2 as u32); return 0; }").unwrap_err();
        assert!(signedness.contains("unknown function 'sadd_sat_u32'"));
    }

    #[test]
    fn rejects_invalid_checked_arithmetic_operands() {
        let count = compile("fn main() -> i32 { let mut result: i32 = 0; let out: ptr = &result; sadd_overflow_i32(1, 2, out, out); return 0; }").unwrap_err();
        assert!(count.contains("expects left value, right value, and an output ptr"));
        let sign = compile("fn main() -> i32 { let mut result: u32 = 0; let out: ptr = &result; sadd_overflow_u32(1 as u32, 2 as u32, out); return 0; }").unwrap_err();
        assert!(sign.contains("unknown function 'sadd_overflow_u32'"));
    }

    #[test]
    fn rejects_wrong_widths_and_invalid_bit_intrinsic_arguments() {
        let width_error = compile("fn main() -> i32 { let x: i32 = ctpop_8(1); return x; }").unwrap_err();
        assert!(width_error.contains("expects an integer with width 8"));
        let count_error = compile("fn main() -> i32 { let x: i32 = ctlz_32(1, false); return x; }").unwrap_err();
        assert!(count_error.contains("expects exactly one integer argument"));
    }

    #[test]
    fn rejects_arguments_to_memory_order_fences() {
        let error = compile("fn main() -> i32 { fence_seq_cst(true); return 0; }").unwrap_err();
        assert!(error.contains("fence_seq_cst expects no arguments"));
    }

    #[test]
    fn supports_volatile_access_for_each_scalar_width() {
        let source = "fn main() -> i32 { let p: ptr = alloc(8); volatile_store_u8(p, 1); volatile_store_i16(ptr_offset(p, 2), 2); volatile_store_u32(ptr_offset(p, 4), 4); print(volatile_load_u8(p)); print(volatile_load_i16(ptr_offset(p, 2))); print(volatile_load_u32(ptr_offset(p, 4))); free(p); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("store volatile i8"));
        assert!(ir.contains("store volatile i16"));
        assert!(ir.contains("store volatile i32"));
        assert!(ir.contains("load volatile i8"));
        assert!(ir.contains("load volatile i16"));
        assert!(ir.contains("load volatile i32"));
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
    fn lowers_enum_variants_and_explicit_discriminants() {
        let source = "enum Color { Red, Green = 4, Blue } fn color_code(color: Color) -> i32 { return color as i32; } fn main() -> i32 { let mut color: Color = Color.Green; print(color); print(color == Color.Green); print(color < Color.Blue); print(size_of(Color)); return color_code(Color.Blue) - 5; }";
        let ir = compile(source).unwrap();
        let color_id = struct_type_id("Color");
        assert!(ir.contains("store i32 4, ptr"));
        assert!(ir.contains("call i32 @color_code(i32 5)"));
        assert!(ir.contains("icmp eq i32"));
        assert!(ir.contains("icmp slt i32"));
        assert!(ir.contains("define i32 @color_code(i32"));
        assert!(ir.contains("sub i32"));
        assert!(ir.contains("@.fmt.i64, i64 4"));
        assert!(!ir.contains(&format!("%struct.{color_id:016x} = type")));
    }

    #[test]
    fn lowers_exhaustive_enum_match_to_llvm_switch() {
        let source = "enum Color { Red, Green, Blue } fn main() -> i32 { let color: Color = Color.Green; match (color) { Color.Red => { print(10); }, Color.Green => { print(20); }, Color.Blue => { print(30); } } return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("switch i32"));
        assert!(ir.contains("match.arm"));
        assert!(ir.contains("match.invalid"));
        assert!(!ir.contains("match.next"));
        assert!(!ir.contains("icmp eq i32"));
        assert!(ir.contains("call void @llvm.trap()"));
    }

    #[test]
    fn lowers_integer_and_boolean_constant_matches_to_switch() {
        let source = "fn main() -> i32 { let value: i32 = 2; match (value) { 1 => { print(10); }, 2 => { print(20); }, _ => { print(30); } } let flag: bool = true; match (flag) { true => { print(1); }, false => { print(0); } } return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("switch i32"));
        assert!(ir.contains("switch i1"));
        assert!(ir.contains("i32 1, label %match.arm"));
        assert!(ir.contains("i1 true, label %match.arm"));
    }

    #[test]
    fn rejects_non_exhaustive_boolean_match_without_fallback() {
        let error = compile("fn main() -> i32 { let flag: bool = true; match (flag) { true => { print(1); } } return 0; }").unwrap_err();
        assert!(error.contains("non-exhaustive match on bool; missing pattern: false"));
        let duplicate = compile("fn main() -> i32 { let flag: bool = true; match (flag) { true => { print(1); }, true => { print(2); } } return 0; }").unwrap_err();
        assert!(duplicate.contains("duplicate boolean pattern 'true'"));
    }

    #[test]
    fn allows_diverging_panic_fallback_in_match_expression() {
        let source = r#"fn decode(code: i32) -> i32 { return match (code) { 0 => 42, _ => panic("invalid code") }; } fn main() -> i32 { print(decode(0)); return 0; }"#;
        let ir = compile(source).unwrap();
        assert!(ir.contains("switch i32"));
        assert!(ir.contains("phi i32"));
        assert!(ir.contains("call void @llvm.trap()"));
        assert!(ir.contains("unreachable"));
    }

    #[test]
    fn lowers_constant_match_expressions_to_switch_and_phi() {
        let source = "fn main() -> i32 { let x: i32 = 2; let value: i32 = match (x) { 1 => 10, 2 => 20, _ => 30 }; print(value); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("switch i32"));
        assert!(ir.contains("phi i32"));
        assert!(!ir.contains("match.expr.next"));
    }

    #[test]
    fn keeps_dynamic_match_expression_patterns_in_comparison_order() {
        let source = "fn main() -> i32 { let x: i32 = 2; let first: i32 = 1; let second: i32 = 2; let value: i32 = match (x) { first => 10, second => 20, _ => 30 }; print(value); return 0; }";
        let ir = compile(source).unwrap();
        assert!(!ir.contains("switch i32"));
        assert!(ir.contains("icmp eq i32"));
        assert!(ir.contains("match.expr.next"));
        assert!(ir.contains("phi i32"));
    }

    #[test]
    fn rejects_non_diverging_void_expression_arms() {
        let source = "fn main() -> i32 { let p: ptr = alloc(4); let value: i32 = match (1) { 1 => free(p), _ => 2 }; return value; }";
        let error = compile(source).unwrap_err();
        assert!(error.contains("match expression arms must produce a value unless they diverge"));
    }

    #[test]
    fn retains_comparison_chain_for_dynamic_match_patterns() {
        let source = "fn main() -> i32 { let value: i32 = 5; let pattern: i32 = 5; match (value) { pattern => { print(1); }, _ => { print(0); } } return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("icmp eq i32"));
        assert!(ir.contains("match.next"));
        assert!(!ir.contains("switch i32"));
    }

    #[test]
    fn supports_wildcard_match_fallback_for_integer_values() {
        let source = "fn main() -> i32 { let value: i32 = 5; match (value) { 1 => { print(1); }, 2 => { print(2); }, _ => { print(9); } } return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("match.arm"));
        assert!(ir.contains("call i32 (ptr, ...) @printf(ptr @.fmt.i32, i32 9)"));
    }

    #[test]
    fn rejects_non_exhaustive_and_duplicate_enum_match_arms() {
        let missing = compile("enum Color { Red, Green, Blue } fn main() -> i32 { let color: Color = Color.Red; match (color) { Color.Red => { print(1); }, Color.Green => { print(2); } } return 0; }").unwrap_err();
        assert!(missing.contains("non-exhaustive match on enum 'Color'"));
        let duplicate = compile("enum Color { Red, Green } fn main() -> i32 { let color: Color = Color.Red; match (color) { Color.Red => { print(1); }, Color.Red => { print(2); } } return 0; }").unwrap_err();
        assert!(duplicate.contains("duplicate discriminant 0 in match on enum 'Color'"));
        let non_enum = compile("fn main() -> i32 { let value: i32 = 1; match (value) { 1 => { print(1); } } return 0; }").unwrap_err();
        assert!(non_enum.contains("match on non-enum values requires a '_' fallback arm"));
    }

    #[test]
    fn lowers_enum_variants_in_global_constants_and_static_storage() {
        let source = "enum State { Idle, Running = 4, Halted } const START: State = State.Running; static mut CURRENT: State = State.Idle; fn main() -> i32 { print(START); print(CURRENT); CURRENT = State.Halted; print(CURRENT); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("@START = constant i32 4"));
        assert!(ir.contains("@CURRENT = global i32 0"));
        assert!(ir.contains("store i32 5, ptr @CURRENT"));
        assert!(ir.contains("load i32, ptr @CURRENT"));
    }

    #[test]
    fn resolves_enum_types_across_source_files() {
        let mut main = parse_source("fn read_color(color: Color) -> i32 { return color as i32; } fn main() -> i32 { let color: Color = Color.Blue; return read_color(color) - 2; }").unwrap();
        let colors = parse_source("enum Color { Red, Green, Blue }").unwrap();
        main.functions.append(&mut colors.functions.clone());
        main.structs.append(&mut colors.structs.clone());
        main.enums.extend(colors.enums.clone());
        main.constants.extend(colors.constants.clone());
        main.static_vars.extend(colors.static_vars.clone());
        for (id, name) in colors.type_references {
            main.type_references.insert(id, name);
        }
        let ir = compile_to_llvm(&main).unwrap();
        assert!(ir.contains("define i32 @read_color(i32"));
        assert!(ir.contains("sub i32"));
    }

    #[test]
    fn rejects_duplicate_enum_variants_and_unknown_enum_types() {
        let duplicate = parse_source("enum Color { Red, Red } fn main() -> i32 { return 0; }").unwrap_err();
        assert!(duplicate.contains("duplicate variant 'Red'"));
        let missing = compile("fn main() -> i32 { let color: MissingColor = MissingColor.Red; return 0; }").unwrap_err();
        assert!(missing.contains("unknown type 'MissingColor'"));
        let variant = compile("enum Color { Red, Blue } fn main() -> i32 { let color: Color = Color.Green; return 0; }").unwrap_err();
        assert!(variant.contains("enum 'Color' has no variant 'Green'"));
    }

    #[test]
    fn allows_explicit_enum_value_after_i32_maximum() {
        let ir = compile("enum Wide { Max = 2147483647, Reset = -3, Next } fn main() -> i32 { print(Wide.Max); print(Wide.Reset); print(Wide.Next); return 0; }").unwrap();
        assert!(ir.contains("printf(ptr @.fmt.i32, i32 2147483647)"));
        assert!(ir.contains("printf(ptr @.fmt.i32, i32 -3)"));
        assert!(ir.contains("printf(ptr @.fmt.i32, i32 -2)"));
    }

    #[test]
    fn rejects_implicit_enum_discriminant_overflow() {
        let error = parse_source("enum Wide { Max = 2147483647, Overflow } fn main() -> i32 { return 0; }").unwrap_err();
        assert!(error.contains("enum discriminant overflow before implicit variant"));
    }

    #[test]
    fn lowers_named_struct_declarations_and_fields() {
        let source = "struct Point { x: i32, y: i32 } fn main() -> i32 { let mut point: Point = Point { x: 20, y: 22 }; print(point.x + point.y); point.x = 30; print(point.x); return 0; }";
        let ir = compile(source).unwrap();
        let type_id = struct_type_id("Point");
        assert!(ir.contains(&format!("%struct.{type_id:016x} = type {{ i32, i32 }}")));
        assert!(ir.contains(&format!("getelementptr inbounds %struct.{type_id:016x}")));
        assert!(ir.contains("store i32 30"));
        assert!(ir.contains("add i32"));
    }

    #[test]
    fn lowers_packed_struct_layout_and_unaligned_field_access() {
        let source = "packed struct Packet { tag: u8, value: i32, tail: u16 } fn main() -> i32 { let mut packet: Packet = Packet { tag: 1, value: 42, tail: 7 }; print(offset_of(Packet, value)); print(size_of(Packet)); print(align_of(Packet)); packet.value = 43; print(packet.value); return 0; }";
        let ir = compile(source).unwrap();
        let type_id = struct_type_id("Packet");
        assert!(ir.contains(&format!("%struct.{type_id:016x} = type <{{ i8, i32, i16 }}>")));
        assert!(ir.contains("store i32 42, ptr"));
        assert!(ir.contains("store i32 42, ptr") && ir.contains(", align 1"));
        assert!(ir.contains("load i32, ptr") && ir.contains(", align 1"));
        assert!(ir.contains("store i32 43, ptr") && ir.contains(", align 1"));
    }

    #[test]
    fn lowers_address_of_for_variables_and_fields() {
        let source = "struct Point { x: i32 } fn main() -> i32 { let mut value: i32 = 7; let p: ptr = &value; store_i32(p, 42); let mut point: Point = Point { x: 1 }; let field: ptr = &point.x; store_i32(field, value); print(point.x); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("store ptr %v"));
        assert!(ir.contains("store i32 7, ptr"));
        assert!(ir.contains("getelementptr inbounds %struct."));
    }

    #[test]
    fn lowers_size_of_for_scalar_array_pointer_and_struct_types() {
        let source = "struct Cell { value: i32 } fn main() -> i32 { let a: i64 = size_of(i32); let b: i64 = size_of([i32; 4]); let c: i64 = size_of(ptr); let d: i64 = size_of(Cell); print(a); print(b); print(c); print(d); return 0; }";
        let ir = compile(source).unwrap();
        let cell_id = struct_type_id("Cell");
        assert!(ir.contains("store i64 4, ptr"));
        assert!(ir.contains("store i64 16, ptr"));
        assert!(ir.contains("getelementptr ptr, ptr null, i64 1"));
        assert!(ir.contains(&format!("getelementptr %struct.{cell_id:016x}, ptr null, i64 1")));
        assert!(ir.contains("ptrtoint ptr"));
    }

    #[test]
    fn lowers_target_aware_alignment_for_scalars_arrays_and_structs() {
        let source = "struct Aligned { byte: u8, wide: i64 } fn main() -> i32 { let a: i64 = align_of(i8); let b: i64 = align_of(i32); let c: i64 = align_of([i16; 4]); let d: i64 = align_of(Aligned); print(a); print(b); print(c); print(d); return 0; }";
        let ir = compile(source).unwrap();
        let struct_id = struct_type_id("Aligned");
        assert!(ir.contains("getelementptr { i8, i8 }, ptr null"));
        assert!(ir.contains("getelementptr { i8, i32 }, ptr null"));
        assert!(ir.contains("getelementptr { i8, [4 x i16] }, ptr null"));
        assert!(ir.contains(&format!("getelementptr {{ i8, %struct.{struct_id:016x} }}, ptr null")));
        assert!(ir.matches("ptrtoint ptr").count() >= 4);
    }

    #[test]
    fn lowers_struct_field_offsets_using_llvm_layout() {
        let source = "struct Packet { tag: u8, value: i64, tail: u32 } fn main() -> i32 { let a: i64 = offset_of(Packet, tag); let b: i64 = offset_of(Packet, value); let c: i64 = offsetof(Packet, tail); print(a); print(b); print(c); return 0; }";
        let ir = compile(source).unwrap();
        let type_id = struct_type_id("Packet");
        assert!(ir.contains(&format!("getelementptr %struct.{type_id:016x}, ptr null, i32 0, i32 0")));
        assert!(ir.contains(&format!("getelementptr %struct.{type_id:016x}, ptr null, i32 0, i32 1")));
        assert!(ir.contains(&format!("getelementptr %struct.{type_id:016x}, ptr null, i32 0, i32 2")));
        assert!(ir.matches("ptrtoint ptr").count() >= 3);
    }

    #[test]
    fn rejects_invalid_offset_of_types_and_fields() {
        let non_struct = compile("fn main() -> i32 { let n: i64 = offset_of(i32, value); return 0; }").unwrap_err();
        assert!(non_struct.contains("offset_of requires a struct type"));
        let missing = compile("struct Point { x: i32 } fn main() -> i32 { let n: i64 = offset_of(Point, y); return 0; }").unwrap_err();
        assert!(missing.contains("has no field 'y'"));
    }

    #[test]
    fn rejects_size_of_void() {
        let error = compile("fn main() -> i32 { let n: i64 = size_of(void); return 0; }").unwrap_err();
        assert!(error.contains("size_of(void) is invalid"));
    }

    #[test]
    fn rejects_align_of_void() {
        let error = compile("fn main() -> i32 { let n: i64 = align_of(void); return 0; }").unwrap_err();
        assert!(error.contains("align_of(void) is invalid"));
    }

    #[test]
    fn rejects_address_of_temporary_expressions() {
        let error = compile("fn main() -> i32 { let p: ptr = &(1 + 2); return 0; }").unwrap_err();
        assert!(error.contains("address-of requires a variable, array element, or struct field"));
    }

    #[test]
    fn rejects_invalid_struct_literals_and_fields() {
        let missing = compile("struct Point { x: i32, y: i32 } fn main() -> i32 { let p: Point = Point { x: 1 }; return 0; }").unwrap_err();
        assert!(missing.contains("missing field 'y'"));

        let unknown = compile("struct Point { x: i32 } fn main() -> i32 { let p: Point = Point { z: 1 }; return 0; }").unwrap_err();
        assert!(unknown.contains("has no field 'z'"));

        let immutable = compile("struct Point { x: i32 } fn main() -> i32 { let p: Point = Point { x: 1 }; p.x = 2; return 0; }").unwrap_err();
        assert!(immutable.contains("cannot assign to field 'x' of immutable variable 'p'"));
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
    fn emits_f32_c_abi_parameters_and_returns() {
        let source = "extern fn c_scale(value: f32) -> f32; fn apply(value: f32) -> f32 { return c_scale(value); } fn main() -> i32 { let result: f32 = apply(1.25); print(result); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("declare float @c_scale(float)"));
        assert!(ir.contains("call float @c_scale(float"));
        assert!(ir.contains("define float @apply(float"));
        assert!(ir.contains("call float @apply(float"));
    }

    #[test]
    fn lowers_variadic_external_calls_and_default_promotions() {
        let source = "extern fn variadic_sum(base: i32, ...) -> i32; fn main() -> i32 { let small: i8 = -5; let medium: u16 = 250; let flag: bool = true; return variadic_sum(10, small, medium, flag, 2.0) - 258; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("declare i32 @variadic_sum(i32, ...)"));
        assert!(ir.contains("sext i8"));
        assert!(ir.contains("zext i16"));
        assert!(ir.contains("zext i1"));
        assert!(ir.contains("call i32 (i32, ...) @variadic_sum(i32 10"));
        assert!(ir.contains("double 2.000000"));
    }

    #[test]
    fn allows_declaring_and_calling_printf_as_variadic_ffi() {
        let source = r#"extern fn printf(format: str, ...) -> i32; fn main() -> i32 { let code: i8 = 65; printf("%d %d %.1f\n", code, true, 2.5); return 0; }"#;
        let ir = compile(source).unwrap();
        assert_eq!(ir.matches("declare i32 @printf(ptr, ...)").count(), 1);
        assert!(ir.contains("call i32 (ptr, ...) @printf(ptr"));
        assert!(ir.contains("sext i8"));
        assert!(ir.contains("zext i1"));
        assert!(ir.contains("double 2.500000"));
    }

    #[test]
    fn rejects_incompatible_printf_declarations() {
        let error = compile("extern fn printf(format: str) -> i32; fn main() -> i32 { return 0; }").unwrap_err();
        assert!(error.contains("declare it as extern fn printf(format: str, ...) -> i32"));
    }

    #[test]
    fn rejects_invalid_variadic_declarations_and_calls() {
        let definition = parse_source("fn not_extern(value: i32, ...) -> i32 { return value; } fn main() -> i32 { return 0; }").unwrap_err();
        assert!(definition.contains("variadic functions must be declared with 'extern fn'"));
        let no_fixed = parse_source("extern fn invalid(...) -> i32; fn main() -> i32 { return 0; }").unwrap_err();
        assert!(no_fixed.contains("at least one fixed parameter"));

        let too_few = compile("extern fn variadic_sum(base: i32, ...) -> i32; fn main() -> i32 { return variadic_sum() ; }").unwrap_err();
        assert!(too_few.contains("expects at least 1 argument(s), got 0"));

        let aggregate = compile("struct Pair { a: i32, b: i32 } extern fn consume(base: i32, ...) -> i32; fn main() -> i32 { let p: Pair = Pair { a: 1, b: 2 }; return consume(0, p); }").unwrap_err();
        assert!(aggregate.contains("cannot pass arrays or structs by value"));
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
    fn guards_negative_and_out_of_range_shift_counts() {
        let negative = compile("fn main() -> i32 { let value: i32 = 1; let amount: i32 = -1; return value << amount; }").unwrap();
        assert!(negative.contains("icmp sge i32"));
        assert!(negative.contains("shift.trap"));
        assert!(negative.contains("call void @llvm.trap()"));

        let too_large = compile("fn main() -> i32 { let value: u32 = 1; let amount: u32 = 32; return (value << amount) as i32; }").unwrap();
        assert!(too_large.contains("icmp ult i32"));
        assert!(too_large.contains("shift.trap"));
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
    fn supports_f32_literals_arithmetic_casts_arrays_and_variadic_promotion() {
        let source = r#"const HALF: f32 = 0.5; const TINY: f32 = 1.25e-2; extern fn printf(format: str, ...) -> i32; fn main() -> i32 { let values: [f32; 2] = [1.5, 2.25]; let result: f32 = values[0] * values[1] + HALF; print(result); let wide: f64 = result as f64; let narrow: f32 = wide as f32; print(narrow); printf("%.2f\n", narrow); let integer: i32 = narrow as i32; print(integer); print(size_of([f32; 2])); print(align_of(f32)); print(TINY); return 0; }"#;
        let ir = compile(source).unwrap();
        assert!(ir.contains("@HALF = constant float"));
        assert!(ir.contains("@TINY = constant float"));
        assert!(ir.contains("alloca [2 x float]"));
        assert!(ir.contains("fmul float"));
        assert!(ir.contains("fadd float"));
        assert!(ir.contains("fpext float"));
        assert!(ir.contains("fptrunc double"));
        assert!(ir.contains("fptosi float"));
        assert!(ir.contains("call i32 (ptr, ...) @printf(ptr"));
        assert!(ir.contains("double"));
        assert!(ir.contains("call i32 (ptr, ...) @printf(ptr @.fmt.i64, i64 8)"));
        assert!(ir.contains("getelementptr { i8, float }, ptr null"));
    }

    #[test]
    fn parses_scientific_float_literals_and_rejects_incomplete_exponents() {
        let ir = compile("fn main() -> i32 { let value: f64 = 1.25e2; let small: f32 = 1.25E-2; print(value); print(small); return 0; }").unwrap();
        assert!(ir.contains("store double 1.25000000000000000e2"));
        assert!(ir.contains("store float"));
        let error = parse_source("fn main() -> i32 { let value: f64 = 1.0e+; return 0; }").unwrap_err();
        assert!(error.contains("floating-point exponent requires at least one digit"));
    }

    #[test]
    fn lowers_f32_global_storage_and_unaligned_volatile_access() {
        let source = "static mut DEVICE_VALUE: f32 = 1.25e-2; fn main() -> i32 { let address: ptr = &DEVICE_VALUE; volatile_store_unaligned_f32(address, 3.5); let value: f32 = volatile_load_unaligned_f32(address); print(value); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("@DEVICE_VALUE = global float"));
        assert!(ir.contains("store volatile float"));
        assert!(ir.contains("load volatile float"));
        assert!(ir.contains("align 1"));
        assert!(ir.contains("fpext float"));
    }

    #[test]
    fn supports_f32_volatile_memory_load_store_variants() {
        let source = "fn main() -> i32 { let memory: ptr = alloc(size_of(f32)); volatile_store_f32(memory, 1.5); let first: f32 = volatile_load_f32(memory); store_unaligned_f32(ptr_offset(memory, 1), first); let second: f32 = load_unaligned_f32(ptr_offset(memory, 1)); print(second); free(memory); return 0; }";
        let ir = compile(source).unwrap();
        assert!(ir.contains("store volatile float"));
        assert!(ir.contains("load volatile float"));
        assert!(ir.contains("store float"));
        assert!(ir.contains("load float"));
        assert!(ir.contains("align 1"));
    }

    #[test]
    fn rejects_f32_non_finite_literals() {
        let error = compile("fn main() -> i32 { let value: f32 = 3402824000000000000000000000000000000000.0; return 0; }").unwrap_err();
        assert!(error.contains("f32 finite range"), "unexpected error: {error}");
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
    fn lowers_native_async_functions_and_typed_future_await() {
        let source = r#"
            async fn add_one(value: i32) -> i32 {
                return value + 1;
            }

            fn main() -> i32 {
                let future: Future<i32> = add_one(41);
                let result: i32 = await future;
                print(result);
                return 0;
            }
        "#;
        let ir = compile(source).unwrap();
        assert!(ir.contains("%__ramlang_async_args_"));
        assert!(ir.contains("@ramlang_future_spawn_i64(ptr @__ramlang_async_entry_"));
        assert!(ir.contains("@ramlang_future_await_i64(ptr"));
        assert!(ir.contains("@ramlang_future_close(ptr"));
        assert!(ir.contains("trunc i64"));
    }

    #[test]
    fn rejects_awaiting_a_non_future_value() {
        let error = compile("fn main() -> i32 { let result: i32 = await 41; return 0; }").unwrap_err();
        assert!(error.contains("await expects Future<T>"), "unexpected error: {error}");
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
