use super::{Line, LineKind, Span};

// all the parse methods that we use to parse a LineKind like parse_section or parse_variable
// do not return LineKind but the information of the kind they are parsing. We have seen this with
// jolt and parse_array(). We didnt not return Value but Vec<Value> it is correct semmanticly. By
// returning a LineKind someone could assume that we could return any kind of Line.
#[derive(Debug)]
pub(crate) struct ParseError {
    pub kind: ParseErrorKind,
    pub pos: usize, // line-relative offset
}

#[derive(Debug)]
pub(crate) enum ParseErrorKind {
    UnexpectedByte(u8),
    UnterminatedQuote,
    UnexpectedEof,
}

pub(super) struct LineParser<'a> {
    buf: &'a [u8],
    pos: usize,
    span: Span,
}

impl<'a> LineParser<'a> {
    pub(super) fn new(buf: &'a [u8], span: Span) -> Self {
        Self {
            buf: &buf[span.start..span.end],
            pos: 0,
            span,
        }
    }

    // once we parse a line we are done, this is why we consume self
    // https://git-scm.com/docs/git-config#_syntax
    //
    // we skip ws at the start of the line and then only between different tokens within the line.
    // For a variable, we skip ws to parse the name(1st token) then we skip to parse the optional
    // '='(2nd token) and the remaining is the value. We never skip ws after parsing the value.
    pub(super) fn parse(mut self) -> Result<Line, ParseError> {
        self.skip_ws();

        match self.peek() {
            Some(b'[') => {
                let kind = self.parse_section()?;
                self.check_trailing_comment()?;
                Ok(Line {
                    raw: self.span,
                    kind: LineKind::Section { name: kind.0, subsection: kind.1 },
                })
            }
            // we never scan the comment if we detect it, it spans until the end of the line
            // comments can't be multiline
            Some(b'#' | b';') => Ok(Line { raw: self.span, kind: LineKind::Comment, }),
            Some(byte) if byte.is_ascii_alphabetic() => {
                let variable = self.parse_variable()?;
                Ok(Line { raw: self.span, kind: LineKind::Variable { 
                    name: variable.0, 
                    value: variable.1 
                }})
            },
            // if after skipping ws we are at the end of line we have a blank line,
            // if the last line is blank then there is no '\n' at the end of the line so peek() returns
            // none, for any other line it returns Some(b'\n') or Some(b'/r') depending on the OS
            //
            // Windows use CRLF. (Carriage Return + Line Feed) files use the \r\n invisible character
            // sequence to denote the end of a line. Unix uses LF and macOS uses CR
            // https://stackoverflow.com/questions/1552749/difference-between-cr-lf-lf-and-cr-line-break-types
            // When we read the file, we are more permissive. A user may edit .lit/config manually
            // with an editor that writes CRLF. When we write we always emit LF
            Some(b'\r' | b'\n') | None => Ok(Line { raw: self.span, kind: LineKind::Blank, }),
            Some(byte) => Err(ParseError {
                pos: self.pos,
                kind: ParseErrorKind::UnexpectedByte(*byte),
            }),
        }
    }

    fn parse_section(&mut self) -> Result<(Span, Option<Span>), ParseError> {
        self.advance(1); // skip opening '['
        let start = self.pos;

        while let Some(b) = self.peek() {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.') {
                self.advance(1);
            } else {
                break;
            }
        }
        let section = Span { start, end: self.pos, };

        // Header has a strict syntax: [section "subsection"]. No leading/trailing whitespaces are
        // allowed, section is separated from subsection by a single space
        match self.peek() {
            Some(b' ') => self.advance(1),
            Some(b']') => {
                self.advance(1); // skip closing ']'
                return Ok((section, None));
            }
            Some(&byte) => {
                return Err(ParseError {
                    pos: self.pos,
                    kind: ParseErrorKind::UnexpectedByte(byte),
                });
            }
            None => return Err(ParseError {
                pos: self.pos,
                kind: ParseErrorKind::UnexpectedEof,
            }),
        }

        let subsection = match self.peek() {
            Some(b']') => {
                self.advance(1);
                None
            },
            Some(b'"') => Some(self.parse_subsection()?),
            Some(&byte) => return Err(ParseError {
                pos: self.pos,
                kind: ParseErrorKind::UnexpectedByte(byte),
            }),
            None => return Err(ParseError {
                pos: self.pos,
                kind: ParseErrorKind::UnexpectedEof,
            }),
        };
        Ok((section, subsection))
    }

    fn parse_subsection(&mut self) -> Result<Span, ParseError> {
        self.advance(1); // skip opening "
        let start = self.pos;

        while let Some(byte) = self.peek() {
            match byte {
                b'\"' => {
                    let span = Span { start, end: self.pos, };
                    self.advance(1);
                    return Ok(span);
                }
                b' ' | b'\t' => {
                    return Err(ParseError {
                        pos: self.pos,
                        kind: ParseErrorKind::UnexpectedByte(*byte),
                    });
                }
                // unpaired
                b'\\' if self.buf.get(self.pos + 1).is_none() => {
                    return Err(ParseError {
                        pos: self.pos,
                        kind: ParseErrorKind::UnexpectedEof,
                    })
                }
                b'\\' => self.advance(2),
                // Git is byte oriented, we could enforce utf8, but we won't,
                // we will display bad sequences with the hex value of each byte
                _ => self.advance(1),
            }
        }
        // never encountered closing '"', should be eof
        Err(ParseError { pos: self.pos, kind: ParseErrorKind::UnexpectedEof, })
    }

    fn check_trailing_comment(&mut self) -> Result<(), ParseError> {
        self.skip_ws();
        match self.peek() {
            // once we see the delimiter for comment it is enough to stop, we never scan the contents
            // everything after the delimiter up to the end of the buffer is part of the comment.
            // \ inside a comment is an ordinary byte. Comments do not fold in the next line
            None | Some(b'#' | b';') => Ok(()),
            Some(&byte) => Err(ParseError {
                pos: self.pos,
                kind: ParseErrorKind::UnexpectedByte(byte),
            }), // junk post closing ']' is an error
        }
    }

    // only values can be multiline so the check for '\' happens in parse_value() everywhere else
    // is an unexpected character.
    fn parse_variable(&mut self) -> Result<(Span, Option<Span>), ParseError> {
        // the 1st character is verified that is alphabetic by the caller
        self.advance(1);
        let start = self.pos;

        // name can contain only alphanumeric characters and '-'
        while let Some(b) = self.peek() {
            if b.is_ascii_alphanumeric() || *b == b'-' {
                self.advance(1);
            } else {
                break;
            }
        }
        let name = Span { start, end: self.pos, };

        self.skip_ws();
        let value = match self.peek() {
            Some(b'=') => {
                self.advance(1);
                self.skip_ws();
                Some(self.parse_value()?)
            }
            // valueless boolean, implicitly true
            Some(b'#') | Some(b';') | None => None,
            Some(&byte) => return Err(ParseError {
                pos: self.pos,
                kind: ParseErrorKind::UnexpectedByte(byte),
            }),
        };
        Ok((name, value))
    }

    // weirdest syntax I have seen so far
    //
    // values are not just one word, they may contain spaces, may be partly quoted(that is insane!!),
    // may contain escapes, and may continue onto the next physical line.
    //
    //  editor =
    //      valid, value is the empty string
    //  editor =        # comment
    //      valid, value is still the empty string
    //  name = John Doe
    //      valid, internal whitespace is retained even when unquoted
    //  name = John; this is a comment
    //      valid, comments stop an unquoted value, to have '#' or ';' as part of the value we need
    //      to use quotes
    //  key = a " b " c
    //      valid, quoted pieces can appear inside a value. The quotes are syntax, they are not
    //      part of the final value. The value becomes when interpreted a b c. We have to support
    //      partially quoted values, not only whole value quoted vs whole value unquoted. This is also
    //      valid key = hello "# not a comment" world (# is part of the value because it is quoted)
    //  name = foo   \
    //            bar
    //      valid, the whitespaces between foo and bar are retained
    //      '\' folds the value into the new line the value after interprtoation is: foo     bar
    //      the '\' and the line ending are removed when we interprete the value,
    //  name = /
    //    value
    //      valid, the value just starts in the next line
    //
    //
    // The optional '\', the optional '\n' are part of the value's span. This is the intented behavior,
    // we want to know the span of the value within the line. When we interprete the value we will
    // discard those. If we tried to discard them when we read the value, the value then should be
    // a Vec<Span> instead.
    // parse_value() must hold the invariance that the span of the value does not include any
    // leading/trailing ws.
    fn parse_value(&mut self) -> Result<Span, ParseError> {
        let start = self.pos;
        let mut in_quote = false;
        let mut last_quote_pos= start;

        // inside quotes essentially every byte value is allowed expect two, everything else is taken
        // verbatim
        // ": to include it we need to escape it
        // \: starts an escape. To include a literal backslash, escape it: \\. It also introduces
        // the valid escapes \n \t \b \" \\ and the continuation rule \<newline>. Similar behavior
        // to json strings
        //
        // the only exception is the new line character as raw byte value. A literal newline byte
        // inside an open quote is an unterminated quote. It does not matter if later we could encounter
        // a closing quote(only possible in a multiline value).
        while let Some(&byte) = self.peek() {
            match byte {
                b'"' => {
                    last_quote_pos = self.pos;
                    in_quote = !in_quote;
                    self.advance(1);
                }
                b'\\' => {
                    self.advance(1);
                    match self.peek() {
                        // everything up to \n are the supported escape sequences
                        // \<newline> is the continuation rule
                        Some(b'n' | b't' | b'b' | b'"' | b'\\' | b'\n') => self.advance(1),
                        Some(&other) => {
                            return Err(ParseError {
                                pos: self.pos,
                                kind: ParseErrorKind::UnexpectedByte(other),
                            });
                        }
                        None => {} // lone '\' at EOF: git drops it
                    }
                }
                // these 2 cases are the boundaries of the value, we either hit a newline or a comment
                b'#' | b';' if !in_quote => break,
                b'\n' | b'\r' => break,
                _ => self.advance(1),
            }
        }
        if in_quote {
            return Err(ParseError { pos: last_quote_pos, kind: ParseErrorKind::UnterminatedQuote });
        }

        // this is the part where we trim the trailing whitespaces
        // we can't do it as we parse the value because we don't know yet if they will be part of the
        // value until we hit a comment or a newline.(the cases where we break above)
        let mut end = self.pos;
        while end >= start && matches!(self.buf[end - 1], b' ' | b'\t') {
            end -= 1;
        }
        Ok(Span { start, end })
    }

    fn advance(&mut self, n: usize) {
        self.pos += n;
    }

    fn peek(&self) -> Option<&u8> {
        self.buf.get(self.pos)
    }

    // read parse()
    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.advance(1);
        }
    }

    fn eol(&self) -> bool {
        matches!(self.peek(), None | Some(b'\n' | b'\r'))
    }
}
