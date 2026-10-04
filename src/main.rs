use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    // Single-character tokens.
    LeftParen,
    RightParen,
    LeftBrace,
    RightBrace,
    Comma,
    Dot,
    Minus,
    Plus,
    Semicolon,
    Slash,
    Star,

    // One or two character tokens.
    Bang,
    BangEqual,
    Equal,
    EqualEqual,
    Greater,
    GreaterEqual,
    Less,
    LessEqual,

    // Literals.
    Identifier,
    String,
    Number,
}

#[derive(Debug, Clone)]
struct Token {
    kind: TokenKind,
    lexeme: String,
    line: usize,
    column: usize,
}

#[derive(Debug)]
enum LexerError {
    InvalidCharacter {
        character: char,
        line: usize,
        column: usize,
    },

    UnterminatedComment {
        line: usize,
        column: usize,
    },

    UnterminatedString {
        line: usize,
        column: usize,
    },
}

impl fmt::Display for LexerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LexerError::InvalidCharacter {
                character,
                line,
                column,
            } => {
                write!(
                    f,
                    "invalid character '{}' at {}:{}",
                    character, line, column
                )
            }

            LexerError::UnterminatedComment { line, column } => {
                write!(
                    f,
                    "unterminated comment at {}:{}",
                    line, column
                )
            }

            LexerError::UnterminatedString { line, column } => {
                write!(
                    f,
                    "unterminated string at {}:{}",
                    line, column
                )
            }
        }
    }
}

impl std::error::Error for LexerError {}

struct Lexer;

impl Lexer {
    fn new() -> Self {
        Self
    }

    fn lex(&self, source: &str) -> Result<TokenStream, LexerError> {
        let bytes = source.as_bytes();

        let mut tokens = Vec::new();

        let mut i = 0usize;
        let mut line = 1usize;
        let mut column = 1usize;

        while i < bytes.len() {
            let start = i;
            let start_line = line;
            let start_column = column;

            let c = bytes[i] as char;

            match c {
                // Single-character tokens.
                '(' => {
                    i += 1;
                    column += 1;

                    Self::append_token(
                        &mut tokens,
                        TokenKind::LeftParen,
                        &source[start..i],
                        start_line,
                        start_column,
                    );
                }

                ')' => {
                    i += 1;
                    column += 1;

                    Self::append_token(
                        &mut tokens,
                        TokenKind::RightParen,
                        &source[start..i],
                        start_line,
                        start_column,
                    );
                }

                '{' => {
                    i += 1;
                    column += 1;

                    Self::append_token(
                        &mut tokens,
                        TokenKind::LeftBrace,
                        &source[start..i],
                        start_line,
                        start_column,
                    );
                }

                '}' => {
                    i += 1;
                    column += 1;

                    Self::append_token(
                        &mut tokens,
                        TokenKind::RightBrace,
                        &source[start..i],
                        start_line,
                        start_column,
                    );
                }

                ',' => {
                    i += 1;
                    column += 1;

                    Self::append_token(
                        &mut tokens,
                        TokenKind::Comma,
                        &source[start..i],
                        start_line,
                        start_column,
                    );
                }

                '.' => {
                    if i + 1 < bytes.len() && is_digit(bytes[i + 1]) {
                        i += 1;
                        column += 1;

                        while i < bytes.len() && is_digit(bytes[i]) {
                            i += 1;
                            column += 1;
                        }

                        Self::append_token(
                            &mut tokens,
                            TokenKind::Number,
                            &source[start..i],
                            start_line,
                            start_column,
                        );
                    } else {
                        i += 1;
                        column += 1;

                        Self::append_token(
                            &mut tokens,
                            TokenKind::Dot,
                            &source[start..i],
                            start_line,
                            start_column,
                        );
                    }
                }

                '-' => {
                    i += 1;
                    column += 1;

                    Self::append_token(
                        &mut tokens,
                        TokenKind::Minus,
                        &source[start..i],
                        start_line,
                        start_column,
                    );
                }

                '+' => {
                    i += 1;
                    column += 1;

                    Self::append_token(
                        &mut tokens,
                        TokenKind::Plus,
                        &source[start..i],
                        start_line,
                        start_column,
                    );
                }

                ';' => {
                    i += 1;
                    column += 1;

                    Self::append_token(
                        &mut tokens,
                        TokenKind::Semicolon,
                        &source[start..i],
                        start_line,
                        start_column,
                    );
                }

                '*' => {
                    i += 1;
                    column += 1;

                    Self::append_token(
                        &mut tokens,
                        TokenKind::Star,
                        &source[start..i],
                        start_line,
                        start_column,
                    );
                }

                // Slash or comment.
                '/' => {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                        i += 2;
                        column += 2;

                        while i < bytes.len() && bytes[i] != b'\n' {
                            i += 1;
                            column += 1;
                        }
                    } else if i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                        i += 2;
                        column += 2;

                        let mut terminated = false;

                        while i < bytes.len() {
                            if bytes[i] == b'*'
                                && i + 1 < bytes.len()
                                && bytes[i + 1] == b'/'
                            {
                                i += 2;
                                column += 2;
                                terminated = true;
                                break;
                            }

                            if bytes[i] == b'\n' {
                                i += 1;
                                line += 1;
                                column = 1;
                            } else {
                                i += 1;
                                column += 1;
                            }
                        }

                        if !terminated {
                            return Err(LexerError::UnterminatedComment {
                                line: start_line,
                                column: start_column,
                            });
                        }
                    } else {
                        i += 1;
                        column += 1;

                        Self::append_token(
                            &mut tokens,
                            TokenKind::Slash,
                            &source[start..i],
                            start_line,
                            start_column,
                        );
                    }
                }

                // Bang.
                '!' => {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                        i += 2;
                        column += 2;

                        Self::append_token(
                            &mut tokens,
                            TokenKind::BangEqual,
                            &source[start..i],
                            start_line,
                            start_column,
                        );
                    } else {
                        i += 1;
                        column += 1;

                        Self::append_token(
                            &mut tokens,
                            TokenKind::Bang,
                            &source[start..i],
                            start_line,
                            start_column,
                        );
                    }
                }

                // Equal.
                '=' => {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                        i += 2;
                        column += 2;

                        Self::append_token(
                            &mut tokens,
                            TokenKind::EqualEqual,
                            &source[start..i],
                            start_line,
                            start_column,
                        );
                    } else {
                        i += 1;
                        column += 1;

                        Self::append_token(
                            &mut tokens,
                            TokenKind::Equal,
                            &source[start..i],
                            start_line,
                            start_column,
                        );
                    }
                }

                // Greater.
                '>' => {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                        i += 2;
                        column += 2;

                        Self::append_token(
                            &mut tokens,
                            TokenKind::GreaterEqual,
                            &source[start..i],
                            start_line,
                            start_column,
                        );
                    } else {
                        i += 1;
                        column += 1;

                        Self::append_token(
                            &mut tokens,
                            TokenKind::Greater,
                            &source[start..i],
                            start_line,
                            start_column,
                        );
                    }
                }

                // Less.
                '<' => {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                        i += 2;
                        column += 2;

                        Self::append_token(
                            &mut tokens,
                            TokenKind::LessEqual,
                            &source[start..i],
                            start_line,
                            start_column,
                        );
                    } else {
                        i += 1;
                        column += 1;

                        Self::append_token(
                            &mut tokens,
                            TokenKind::Less,
                            &source[start..i],
                            start_line,
                            start_column,
                        );
                    }
                }

                // Whitespace.
                ' ' | '\t' | '\r' => {
                    i += 1;
                    column += 1;
                }

                '\n' => {
                    i += 1;
                    line += 1;
                    column = 1;
                }

                // String.
                '"' => {
                    i += 1;
                    column += 1;

                    let mut terminated = false;

                    while i < bytes.len() {
                        if bytes[i] == b'\\' {
                            if i + 1 >= bytes.len() {
                                return Err(LexerError::UnterminatedString {
                                    line: start_line,
                                    column: start_column,
                                });
                            }

                            i += 2;
                            column += 2;
                            continue;
                        }

                        if bytes[i] == b'"' {
                            i += 1;
                            column += 1;
                            terminated = true;
                            break;
                        }

                        if bytes[i] == b'\n' {
                            i += 1;
                            line += 1;
                            column = 1;
                        } else {
                            i += 1;
                            column += 1;
                        }
                    }

                    if !terminated {
                        return Err(LexerError::UnterminatedString {
                            line: start_line,
                            column: start_column,
                        });
                    }

                    Self::append_token(
                        &mut tokens,
                        TokenKind::String,
                        &source[start..i],
                        start_line,
                        start_column,
                    );
                }

                // Number.
                '0'..='9' => {
                    i += 1;
                    column += 1;

                    while i < bytes.len() && is_digit(bytes[i]) {
                        i += 1;
                        column += 1;
                    }

                    // Fractional part.
                    if i + 1 < bytes.len()
                        && bytes[i] == b'.'
                        && is_digit(bytes[i + 1])
                    {
                        i += 1;
                        column += 1;

                        while i < bytes.len() && is_digit(bytes[i]) {
                            i += 1;
                            column += 1;
                        }
                    }

                    Self::append_token(
                        &mut tokens,
                        TokenKind::Number,
                        &source[start..i],
                        start_line,
                        start_column,
                    );
                }

                // Identifier.
                'a'..='z' | 'A'..='Z' | '_' => {
                    i += 1;
                    column += 1;

                    while i < bytes.len()
                        && is_identifier_continue(bytes[i])
                    {
                        i += 1;
                        column += 1;
                    }

                    Self::append_token(
                        &mut tokens,
                        TokenKind::Identifier,
                        &source[start..i],
                        start_line,
                        start_column,
                    );
                }

                _ => {
                    return Err(LexerError::InvalidCharacter {
                        character: c,
                        line,
                        column,
                    });
                }
            }
        }

        Ok(TokenStream::new(tokens))
    }

    fn append_token(
        tokens: &mut Vec<Token>,
        kind: TokenKind,
        lexeme: &str,
        line: usize,
        column: usize,
    ) {
        tokens.push(Token {
            kind,
            lexeme: lexeme.to_owned(),
            line,
            column,
        });
    }
}

fn is_digit(c: u8) -> bool {
    c.is_ascii_digit()
}

fn is_identifier_continue(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

struct TokenStream {
    tokens: Vec<Token>,
    index: usize,
}

impl TokenStream {
    fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            index: 0,
        }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.index)
    }

    fn advance(&mut self) -> Option<Token> {
        if self.index >= self.tokens.len() {
            return None;
        }

        let token = self.tokens[self.index].clone();
        self.index += 1;

        Some(token)
    }

    fn is_at_end(&self) -> bool {
        self.index >= self.tokens.len()
    }
}

#[derive(Debug)]
enum Expr {
    Number(String),

    String(String),

    Identifier(String),

    Unary {
        operator: TokenKind,
        right: Box<Expr>,
    },

    Binary {
        left: Box<Expr>,
        operator: TokenKind,
        right: Box<Expr>,
    },

    Grouping(Box<Expr>),
}

#[derive(Debug)]
enum Stmt {
    Expression(Box<Expr>),

    Block {
        statements: Vec<Stmt>,
    },
}

#[derive(Debug)]
struct Ast {
    root: Option<Stmt>,
}

impl Ast {
    fn new() -> Self {
        Self { root: None }
    }
}

#[derive(Debug)]
enum ParseError {
    UnexpectedEndOfInput,

    ExpectedExpression,

    ExpectedSemicolon,

    ExpectedRightParen,

    ExpectedRightBrace,

    UnterminatedBlock,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::UnexpectedEndOfInput => {
                write!(f, "unexpected end of input")
            }

            ParseError::ExpectedExpression => {
                write!(f, "expected expression")
            }

            ParseError::ExpectedSemicolon => {
                write!(f, "expected ';'")
            }

            ParseError::ExpectedRightParen => {
                write!(f, "expected ')'")
            }

            ParseError::ExpectedRightBrace => {
                write!(f, "expected '}}'")
            }

            ParseError::UnterminatedBlock => {
                write!(f, "unterminated block")
            }
        }
    }
}

impl std::error::Error for ParseError {}

struct Parser {
    tokens: TokenStream,
}

impl Parser {
    fn new(tokens: TokenStream) -> Self {
        Self { tokens }
    }

    fn parse(&mut self, ast: &mut Ast) -> Result<(), ParseError> {
        let mut statements = Vec::new();

        while !self.tokens.is_at_end() {
            statements.push(self.parse_statement()?);
        }

        ast.root = Some(Stmt::Block { statements });

        Ok(())
    }

    fn parse_statement(&mut self) -> Result<Stmt, ParseError> {
        if self.match_kind(TokenKind::LeftBrace) {
            return self.parse_block();
        }

        let expr = self.expression()?;

        self.consume(
            TokenKind::Semicolon,
            ParseError::ExpectedSemicolon,
        )?;

        Ok(Stmt::Expression(Box::new(expr)))
    }

    fn parse_block(&mut self) -> Result<Stmt, ParseError> {
        let mut statements = Vec::new();

        while !self.tokens.is_at_end()
            && !self.check(TokenKind::RightBrace)
        {
            statements.push(self.parse_statement()?);
        }

        if self.tokens.is_at_end() {
            return Err(ParseError::UnterminatedBlock);
        }

        self.consume(
            TokenKind::RightBrace,
            ParseError::ExpectedRightBrace,
        )?;

        Ok(Stmt::Block { statements })
    }

    fn expression(&mut self) -> Result<Expr, ParseError> {
        self.equality()
    }

    fn equality(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.comparison()?;

        loop {
            let operator = match self.tokens.peek().map(|token| token.kind) {
                Some(TokenKind::BangEqual) => TokenKind::BangEqual,
                Some(TokenKind::EqualEqual) => TokenKind::EqualEqual,
                _ => break,
            };

            self.tokens.advance();

            let right = self.comparison()?;

            expr = Expr::Binary {
                left: Box::new(expr),
                operator,
                right: Box::new(right),
            };
        }

        Ok(expr)
    }

    fn comparison(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.term()?;

        loop {
            let operator = match self.tokens.peek().map(|token| token.kind) {
                Some(TokenKind::Greater) => TokenKind::Greater,
                Some(TokenKind::GreaterEqual) => TokenKind::GreaterEqual,
                Some(TokenKind::Less) => TokenKind::Less,
                Some(TokenKind::LessEqual) => TokenKind::LessEqual,
                _ => break,
            };

            self.tokens.advance();

            let right = self.term()?;

            expr = Expr::Binary {
                left: Box::new(expr),
                operator,
                right: Box::new(right),
            };
        }

        Ok(expr)
    }

    fn term(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.factor()?;

        loop {
            let operator = match self.tokens.peek().map(|token| token.kind) {
                Some(TokenKind::Minus) => TokenKind::Minus,
                Some(TokenKind::Plus) => TokenKind::Plus,
                _ => break,
            };

            self.tokens.advance();

            let right = self.factor()?;

            expr = Expr::Binary {
                left: Box::new(expr),
                operator,
                right: Box::new(right),
            };
        }

        Ok(expr)
    }

    fn factor(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.unary()?;

        loop {
            let operator = match self.tokens.peek().map(|token| token.kind) {
                Some(TokenKind::Slash) => TokenKind::Slash,
                Some(TokenKind::Star) => TokenKind::Star,
                _ => break,
            };

            self.tokens.advance();

            let right = self.unary()?;

            expr = Expr::Binary {
                left: Box::new(expr),
                operator,
                right: Box::new(right),
            };
        }

        Ok(expr)
    }

    fn unary(&mut self) -> Result<Expr, ParseError> {
        let token_kind = match self.tokens.peek() {
            Some(token) => token.kind,
            None => return Err(ParseError::UnexpectedEndOfInput),
        };

        let operator = match token_kind {
            TokenKind::Bang => TokenKind::Bang,
            TokenKind::Minus => TokenKind::Minus,

            _ => {
                return self.primary();
            }
        };

        self.tokens.advance();

        let right = self.unary()?;

        Ok(Expr::Unary {
            operator,
            right: Box::new(right),
        })
    }

    fn primary(&mut self) -> Result<Expr, ParseError> {
        let token = match self.tokens.advance() {
            Some(token) => token,
            None => return Err(ParseError::UnexpectedEndOfInput),
        };

        match token.kind {
            TokenKind::Number => {
                Ok(Expr::Number(token.lexeme))
            }

            TokenKind::String => {
                Ok(Expr::String(token.lexeme))
            }

            TokenKind::Identifier => {
                Ok(Expr::Identifier(token.lexeme))
            }

            TokenKind::LeftParen => {
                let expr = self.expression()?;

                self.consume(
                    TokenKind::RightParen,
                    ParseError::ExpectedRightParen,
                )?;

                Ok(Expr::Grouping(Box::new(expr)))
            }

            _ => Err(ParseError::ExpectedExpression),
        }
    }

    fn match_kind(&mut self, kind: TokenKind) -> bool {
        if !self.check(kind) {
            return false;
        }

        self.tokens.advance();
        true
    }

    fn check(&self, kind: TokenKind) -> bool {
        match self.tokens.peek() {
            Some(token) => token.kind == kind,
            None => false,
        }
    }

    fn consume(
        &mut self,
        kind: TokenKind,
        error: ParseError,
    ) -> Result<Token, ParseError> {
        match self.tokens.peek() {
            Some(token) if token.kind == kind => {
                Ok(self.tokens.advance().unwrap())
            }

            _ => Err(error),
        }
    }
}

fn print_ast(stmt: &Stmt, indent: usize) {
    let padding = " ".repeat(indent);

    match stmt {
        Stmt::Expression(expr) => {
            println!("{padding}Expression:");
            print_expr(expr, indent + 4);
        }

        Stmt::Block { statements } => {
            println!("{padding}Block:");

            for statement in statements {
                print_ast(statement, indent + 4);
            }
        }
    }
}

fn print_expr(expr: &Expr, indent: usize) {
    let padding = " ".repeat(indent);

    match expr {
        Expr::Number(value) => {
            println!("{padding}Number({value})");
        }

        Expr::String(value) => {
            println!("{padding}String({value})");
        }

        Expr::Identifier(value) => {
            println!("{padding}Identifier({value})");
        }

        Expr::Unary { operator, right } => {
            println!(
                "{padding}Unary({operator:?})"
            );

            print_expr(right, indent + 4);
        }

        Expr::Binary {
            left,
            operator,
            right,
        } => {
            println!(
                "{padding}Binary({operator:?})"
            );

            println!("{padding}Left:");
            print_expr(left, indent + 4);

            println!("{padding}Right:");
            print_expr(right, indent + 4);
        }

        Expr::Grouping(expr) => {
            println!("{padding}Grouping:");
            print_expr(expr, indent + 4);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = r#"{
    x + 10;
    {
        y * 2;
    }
}"#;

    let lexer = Lexer::new();

    let token_stream = lexer.lex(source)?;

    for token in &token_stream.tokens {
        println!(
            "{:?} \"{}\" ({}:{})",
            token.kind,
            token.lexeme,
            token.line,
            token.column
        );
    }

    let mut parser = Parser::new(token_stream);

    let mut ast = Ast::new();

    parser.parse(&mut ast)?;

    println!();
    println!("Parsing successful.");
    println!();

    if let Some(root) = &ast.root {
        print_ast(root, 0);
    }

    Ok(())
}