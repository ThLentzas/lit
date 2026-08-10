use super::LineSpan;

pub(super) struct LineParser<'a> {
    buf: &'a [u8],
    pos: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Header {
    pub(super) name: LineSpan,
    pub(super) subsection: Option<LineSpan>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum LineKind {
    Blank,
    Comment,
    // the term section has different depending on the context
    // for the parser is the header, the section name plus optional subsection. The variables that
    // follow aren't part of the section token, they're separate variable lines that happen to sit
    // under it. That is the whole point of this line oriented approach, we group by line.
    //
    // in the docs section means the header plus all the variables that belong to it, the whole block
    // from one header until the next header (or EOF).
    Header(Header),
    Variable { name: LineSpan, value: Option<LineSpan> },
}

// LineParser parses a single logical line using spans that are relative to that line's slice.
// The Config buffer, however, stores the whole file, and all CST spans should point into that
// shared buffer. Shift every parsed child span by the line's starting offset so `name`,
// `subsection`, and `value` become absolute spans into the full input buffer.


impl<'a> LineParser<'a> {
    pub(super) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    // once we parse a line we are done, this is why we consume self
    // https://git-scm.com/docs/git-config#_syntax
    //
    // we skip ws at the start of the line and then only between different tokens within the line.
    // For a variable, we skip ws to parse the name(1st token) then we skip to parse the optional
    // '='(2nd token) and the remaining is the value. We never skip ws after parsing the value.
    pub(super) fn parse(mut self) -> Result<LineKind, ParseError> {
        self.skip_ws();

        match self.peek() {
            Some(b'[') => {
                let kind = self.parse_header()?;
                self.check_trailing_comment()?;
                Ok(LineKind::Header(Header {
                    name: kind.0,
                    subsection: kind.1,
                }))
            }
            Some(byte) if byte.is_ascii_alphabetic() => {
                let variable = self.parse_variable()?;
                Ok(LineKind::Variable {
                    name: variable.0,
                    value: variable.1,
                })
            }
            // we never scan the comment if we detect it, it spans until the end of the line
            // comments can't be multiline
            Some(b'#' | b';') => Ok(LineKind::Comment),
            // if after skipping ws we are at the end of line we have a blank line,
            // if the last line is blank then there is no '\n' at the end of the line so peek() returns
            // none, for any other line it returns Some(b'\n') or Some(b'/r') depending on the OS
            //
            // Windows use CRLF. (Carriage Return + Line Feed) files use the \r\n invisible character
            // sequence to denote the end of a line. Unix uses LF and macOS uses CR
            // https://stackoverflow.com/questions/1552749/difference-between-cr-lf-lf-and-cr-line-break-types
            // When we read the file, we are more permissive. A user may edit .lit/config manually
            // with an editor that writes CRLF. When we write we always emit LF
            Some(b'\r' | b'\n') | None => Ok(LineKind::Blank),
            Some(&byte) => Err(ParseError {
                pos: self.pos,
                kind: ParseErrorKind::UnexpectedByte(byte),
            }),
        }
    }

    fn parse_header(&mut self) -> Result<(LineSpan, Option<LineSpan>), ParseError> {
        self.advance(1); // skip opening '['
        let start = self.pos;

        while let Some(&b) = self.peek() {
            // Git supports also '.' as a value for the header name, we don't. Read config.rs
            if b.is_ascii_alphanumeric() || b == b'-' {
                self.advance(1);
            } else {
                break;
            }
        }

        let section = LineSpan {
            start,
            end: self.pos,
        };
        // [] empty section name not allowed
        if let Some(b']') = self.peek() {
            return if start == self.pos {
                Err(ParseError {
                    pos: self.pos,
                    kind: ParseErrorKind::UnexpectedByte(b']'),
                })
            } else {
                self.advance(1); // skip closing ']'
                Ok((section, None))
            };
        }
        // Header has a strict syntax: [section "subsection"]. No leading/trailing whitespaces are
        // allowed, section is separated from subsection by a single space
        self.expect(b' ')?;
        // [core ] is allowed
        if let Some(b']') = self.peek() {
            self.advance(1);
            return Ok((section, None));
        };
        self.expect(b'\"')?;
        let subsection = self.parse_subsection()?;
        self.expect(b']')?;

        Ok((section, Some(subsection)))
    }

    fn parse_subsection(&mut self) -> Result<LineSpan, ParseError> {
        let start = self.pos;

        // ws are allowed within ""
        while let Some(&byte) = self.peek() {
            match byte {
                b'\"' => {
                    let span = LineSpan {
                        start,
                        end: self.pos,
                    };
                    self.advance(1);
                    return Ok(span);
                }
                b'\\' => {
                    self.advance(1);
                    // unpaired
                    if self.eol() {
                        return Err(ParseError {
                            pos: self.pos,
                            kind: ParseErrorKind::UnterminatedQuote,
                        });
                    }
                    self.advance(1);
                }
                // Git is byte oriented, we could enforce utf8, but we won't,
                // we will display bad sequences with the hex value of each byte
                _ => self.advance(1),
            }
        }
        // never encountered closing '"'
        Err(ParseError {
            pos: start - 1, // position of the opening quote
            kind: ParseErrorKind::UnterminatedQuote,
        })
    }

    // only values can be multiline so the check for '\' happens in parse_value() everywhere else
    // is an unexpected character.
    // TODO: this should be a struct Variable similar to Header but with different semantics
    fn parse_variable(&mut self) -> Result<(LineSpan, Option<LineSpan>), ParseError> {
        let start = self.pos;
        // the 1st character is verified that is alphabetic by the caller
        self.advance(1);

        // name can contain only alphanumeric characters and '-'
        while let Some(&b) = self.peek() {
            if b.is_ascii_alphanumeric() || b == b'-' {
                self.advance(1);
            } else {
                break;
            }
        }
        let name = LineSpan {
            start,
            end: self.pos,
        };

        self.skip_ws();
        let value = match self.peek() {
            Some(b'=') => {
                self.advance(1);
                self.skip_ws();
                Some(self.parse_value()?)
            }
            // valueless boolean, implicitly true
            Some(b'#' | b';' | b'\r' | b'\n') | None => None,
            Some(&byte) => {
                return Err(ParseError {
                    pos: self.pos,
                    kind: ParseErrorKind::UnexpectedByte(byte),
                });
            }
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
    fn parse_value(&mut self) -> Result<LineSpan, ParseError> {
        // quotes and /<newline> are included if present, will be dropped later when we interpret the
        // value
        let start = self.pos;
        let mut in_quote = false;
        let mut last_quote_pos = start;

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
                        // \r could mean that <newline> is represented as CRLF
                        Some(b'\r') if matches!(self.peek(), Some(b'\n')) => self.advance(2),
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
                b if b == b'\n' || b == b'\r' && matches!(self.peek(), Some(b'\n')) => break,
                _ => self.advance(1),
            }
        }
        if in_quote {
            return Err(ParseError {
                pos: last_quote_pos,
                kind: ParseErrorKind::UnterminatedQuote,
            });
        }

        // this is the part where we trim the trailing whitespaces
        // we can't do it as we parse the value because we don't know yet if they will be part of the
        // value until we hit a comment or a newline.(the cases where we break above)
        let mut end = self.pos;
        while end > start && matches!(self.buf[end - 1], b' ' | b'\t') {
            end -= 1;
        }
        Ok(LineSpan { start, end })
    }

    fn check_trailing_comment(&mut self) -> Result<(), ParseError> {
        self.skip_ws();
        // another implementation would be to call eol() after skip_ws() and then instead of calling
        // peek() we index into the buffer since we know we are in bounds
        if self.eol() || matches!(self.peek(), Some(b'#') | Some(b';')) {
            Ok(())
        } else {
            // once we see the delimiter for comment or eol it is enough to stop, we never scan the
            // contents everything after the delimiter up to the end of the buffer is part of the
            // comment. '\' inside a comment is an ordinary byte. Comments do not fold in the next line
            Err(ParseError {
                pos: self.pos,
                // safe to index since eol() returned false
                kind: ParseErrorKind::UnexpectedByte(self.buf[self.pos]),
            }) // junk post closing ']' is an error
        }
    }

    fn advance(&mut self, n: usize) {
        self.pos += n;
    }

    fn peek(&self) -> Option<&u8> {
        self.buf.get(self.pos)
    }

    fn expect(&mut self, expected: u8) -> Result<(), ParseError> {
        match self.peek() {
            Some(&byte) if byte == expected => {
                self.advance(1);
                Ok(())
            }
            Some(&byte) => Err(ParseError {
                pos: self.pos,
                kind: ParseErrorKind::UnexpectedByte(byte),
            }),
            None => Err(ParseError {
                pos: self.pos,
                kind: ParseErrorKind::UnexpectedEol,
            }),
        }
    }

    // read parse()
    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.advance(1);
        }
    }

    // Our reader pass to LineParser a slice that ends with the new line character(LF), it reads
    // until it encounters '\n'. There are 3 characters that are considered line ending characters
    // LF: '\n', CR: '\r' and CRLF: '\r\n'. CR was used by old MacOS but not anymore, now macOS uses
    // LF. We do not support CR as a stand-alone line ending character. When we write the config back
    // we always add a \n even at the end of the last line, but since the user can temper the file
    // peeking for the last line can result to None, which treat as the eol and not an error.
    fn eol(&self) -> bool {
        match self.peek() {
            None | Some(b'\n') => true,
            Some(b'\r') => matches!(self.buf.get(self.pos + 1), Some(b'\n')),
            _ => false,
        }
    }
}

// all the parse methods that we use to parse a LineKind like parse_section or parse_variable
// do not return LineKind but the information of the kind they are parsing. We have seen this with
// jolt and parse_array(). We did not return Value but Vec<Value> it is correct semmanticly. By
// returning a LineKind someone could assume that we could return any kind of Line.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ParseError {
    pub kind: ParseErrorKind,
    pub pos: usize, // line-relative offset
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ParseErrorKind {
    // TODO: make it expected, got, it is hard to narrow the got value
    UnexpectedByte(u8),
    UnterminatedQuote,
    // this is the equivalent of UnexpectedEof, but we are parsing a line oriented grammar so this
    // name is better?
    UnexpectedEol,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_sections() -> Vec<(&'static [u8], LineKind)> {
        vec![
            // section, no subsection
            (
                b"[core]\n",
                LineKind::Header(Header {
                    name: LineSpan { start: 1, end: 5 },
                    subsection: None,
                }),
            ),
            (
                b"[123]",
                LineKind::Header(Header {
                    name: LineSpan { start: 1, end: 4 },
                    subsection: None,
                }),
            ),
            (
                b"[core]  \t\n",
                LineKind::Header(Header {
                    name: LineSpan { start: 1, end: 5 },
                    subsection: None,
                }),
            ),
            (
                b"[core];trailing comment\n",
                LineKind::Header(Header {
                    name: LineSpan { start: 1, end: 5 },
                    subsection: None,
                }),
            ),
            (
                b"[core]#trailing comment\n",
                LineKind::Header(Header {
                    name: LineSpan { start: 1, end: 5 },
                    subsection: None,
                }),
            ),
            (
                b"[foo-bar]",
                LineKind::Header(Header {
                    name: LineSpan { start: 1, end: 8 },
                    subsection: None,
                }),
            ),
            (
                b"[core ]",
                LineKind::Header(Header {
                    name: LineSpan { start: 1, end: 5 },
                    subsection: None,
                }),
            ),
            // section, subsection
            (
                b"[remote \"origin\"]",
                LineKind::Header(Header {
                    name: LineSpan { start: 1, end: 7 },
                    subsection: Some(LineSpan { start: 9, end: 15 }),
                }),
            ),
            (
                b"[remote \"\"]",
                LineKind::Header(Header {
                    name: LineSpan { start: 1, end: 7 },
                    subsection: Some(LineSpan { start: 9, end: 9 }),
                }),
            ),
            (
                b"[remote \"a\\\"b\"]",
                LineKind::Header(Header {
                    name: LineSpan { start: 1, end: 7 },
                    subsection: Some(LineSpan { start: 9, end: 13 }),
                }),
            ),
            (
                b"[remote \"my origin\"]\n",
                LineKind::Header(Header {
                    name: LineSpan { start: 1, end: 7 },
                    subsection: Some(LineSpan { start: 9, end: 18 }),
                }),
            ),
            (
                b"[remote \"a b ; # c\"]\n",
                LineKind::Header(Header {
                    name: LineSpan { start: 1, end: 7 },
                    subsection: Some(LineSpan { start: 9, end: 18 }),
                }),
            ),
        ]
    }

    fn invalid_sections() -> Vec<(&'static [u8], ParseError)> {
        vec![
            (
                b"[core\n",
                ParseError {
                    pos: 5,
                    kind: ParseErrorKind::UnexpectedByte(b'\n'),
                },
            ),
            (
                b"[core",
                ParseError {
                    pos: 5,
                    kind: ParseErrorKind::UnexpectedEol,
                },
            ),
            // this is tricky, the section name is "re" then we have the mandatory space and when
            // we try to parse the subsection we encounter an unexpected 'm' instead of '"'
            (
                b"[re mote]\n",
                ParseError {
                    pos: 4,
                    kind: ParseErrorKind::UnexpectedByte(b'm'),
                },
            ),
            // expected a space between section-subsection
            (
                b"[core\"x\"]\n",
                ParseError {
                    pos: 5,
                    kind: ParseErrorKind::UnexpectedByte(b'\"'),
                },
            ),
            // never found closing quote
            (
                b"[remote \"origin\n",
                ParseError {
                    pos: 16,
                    kind: ParseErrorKind::UnexpectedEol,
                },
            ),
            // empty section name not allowed
            (
                b"[]",
                ParseError {
                    pos: 1,
                    kind: ParseErrorKind::UnexpectedByte(b']'),
                },
            ),
        ]
    }

    fn valid_variables() -> Vec<(&'static [u8], LineKind)> {
        vec![
            (
                b"key = value\n",
                LineKind::Variable {
                    name: LineSpan { start: 0, end: 3 },
                    value: Some(LineSpan { start: 6, end: 11 }),
                },
            ),
            (
                b"key=value\n",
                LineKind::Variable {
                    name: LineSpan { start: 0, end: 3 },
                    value: Some(LineSpan { start: 4, end: 9 }),
                },
            ),
            // trailing ws
            (
                b"key=value  \t\n",
                LineKind::Variable {
                    name: LineSpan { start: 0, end: 3 },
                    value: Some(LineSpan { start: 4, end: 9 }),
                },
            ),
            // internal spaces are retained
            (
                b"name = John Doe\n",
                LineKind::Variable {
                    name: LineSpan { start: 0, end: 4 },
                    value: Some(LineSpan { start: 7, end: 15 }),
                },
            ),
            // leading/trailing ws are retained within quotes
            (
                b"key = \"  value \"",
                LineKind::Variable {
                    name: LineSpan { start: 0, end: 3 },
                    value: Some(LineSpan { start: 6, end: 16 }),
                },
            ),
            // partial quoting
            (
                b"key = \" a \"b\" c \"",
                LineKind::Variable {
                    name: LineSpan { start: 0, end: 3 },
                    value: Some(LineSpan { start: 6, end: 17 }),
                },
            ),
            (
                b"key = \" # not a comment\"",
                LineKind::Variable {
                    name: LineSpan { start: 0, end: 3 },
                    value: Some(LineSpan { start: 6, end: 24 }),
                },
            ),
            // trailing comment stops the value span
            (
                b"key = \"value\" ; trailing comment",
                LineKind::Variable {
                    name: LineSpan { start: 0, end: 3 },
                    value: Some(LineSpan { start: 6, end: 13 }),
                },
            ),
            // trailing comment stops the value span
            (
                b"key = \"value\" # trailing comment",
                LineKind::Variable {
                    name: LineSpan { start: 0, end: 3 },
                    value: Some(LineSpan { start: 6, end: 13 }),
                },
            ),
            // \t escaped
            (
                b"key = \\t",
                LineKind::Variable {
                    name: LineSpan { start: 0, end: 3 },
                    value: Some(LineSpan { start: 6, end: 8 }),
                },
            ),
            // empty value
            (
                b"key =",
                LineKind::Variable {
                    name: LineSpan { start: 0, end: 3 },
                    value: Some(LineSpan { start: 5, end: 5 }),
                },
            ),
            // empty value with leading ws
            (
                b"key =    \n",
                LineKind::Variable {
                    name: LineSpan { start: 0, end: 3 },
                    value: Some(LineSpan { start: 9, end: 9 }),
                },
            ),
            // empty value followed by comment
            (
                b"key =  ; comment \n",
                LineKind::Variable {
                    name: LineSpan { start: 0, end: 3 },
                    value: Some(LineSpan { start: 7, end: 7 }),
                },
            ),
            // continuation, folded value foo\<nl>bar, the internal ws are retained
            (
                b"key = foo \\\n bar\n",
                LineKind::Variable {
                    name: LineSpan { start: 0, end: 3 },
                    value: Some(LineSpan { start: 6, end: 16 }),
                },
            ),
            // value begins with continuation
            (
                b"key = \\\n bar\n",
                LineKind::Variable {
                    name: LineSpan { start: 0, end: 3 },
                    value: Some(LineSpan { start: 6, end: 12 }),
                },
            ),
            // valueless
            (
                b"flag\n",
                LineKind::Variable {
                    name: LineSpan { start: 0, end: 4 },
                    value: None,
                },
            ),
            (
                b"flag ; comment\n",
                LineKind::Variable {
                    name: LineSpan { start: 0, end: 4 },
                    value: None,
                },
            ),
            // 1st has to be alphabetic
            (
                b"a1-2-3 ; comment\n",
                LineKind::Variable {
                    name: LineSpan { start: 0, end: 6 },
                    value: None,
                },
            ),
        ]
    }

    fn invalid_variables() -> Vec<(&'static [u8], ParseError)> {
        vec![
            (
                b"key! = value\n",
                ParseError {
                    pos: 3,
                    kind: ParseErrorKind::UnexpectedByte(b'!'),
                },
            ),
            // must start with alphabetic
            (
                b"123 = value\n",
                ParseError {
                    pos: 0,
                    kind: ParseErrorKind::UnexpectedByte(b'1'),
                },
            ),
            (
                b"key = \"value",
                ParseError {
                    pos: 6,
                    kind: ParseErrorKind::UnterminatedQuote,
                },
            ),
            // unknown escape
            (
                b"key = \\q",
                ParseError {
                    pos: 7,
                    kind: ParseErrorKind::UnexpectedByte(b'q'),
                },
            ),
        ]
    }

    fn comments() -> Vec<(&'static [u8], LineKind)> {
        vec![
            (b"#hello", LineKind::Comment),
            (b"  #leading ws", LineKind::Comment),
            // empty comment
            (b"#", LineKind::Comment),
            (b"# part of the comment ;", LineKind::Comment),
        ]
    }

    fn blank_lines() -> Vec<(&'static [u8], LineKind)> {
        vec![
            // empty line
            (b"", LineKind::Blank),
            (b"  \t \n", LineKind::Blank),
            // CRLF
            (b"   \r\n", LineKind::Blank),
        ]
    }

    #[test]
    fn test_valid_sections() {
        for (buf, kind) in valid_sections() {
            let parser = LineParser::new(buf);
            let res = parser.parse().unwrap();
            assert_eq!(kind, res, "failed: {:?}", buf);
        }
    }

    #[test]
    fn test_invalid_sections() {
        for (buf, err) in invalid_sections() {
            let parser = LineParser::new(buf);
            let res = parser.parse();
            assert_eq!(res, Err(err), "failed: {:?}", buf);
        }
    }

    #[test]
    fn test_valid_variables() {
        for (buf, kind) in valid_variables() {
            let parser = LineParser::new(buf);
            let res = parser.parse().unwrap();
            assert_eq!(kind, res, "failed: {:?}", buf);
        }
    }

    #[test]
    fn test_invalid_variables() {
        for (buf, err) in invalid_variables() {
            let parser = LineParser::new(buf);
            let res = parser.parse();
            assert_eq!(res, Err(err), "failed: {:?}", buf);
        }
    }

    #[test]
    fn test_comments() {
        for (buf, kind) in comments() {
            let parser = LineParser::new(buf);
            let res = parser.parse().unwrap();
            assert_eq!(kind, res, "failed: {:?}", buf);
        }
    }

    #[test]
    fn test_blank_lines() {
        for (buf, kind) in blank_lines() {
            let parser = LineParser::new(buf);
            let res = parser.parse().unwrap();
            assert_eq!(kind, res, "failed: {:?}", buf);
        }
    }

    #[test]
    fn test_unexpected_character() {
        // @
        let buf = [64];
        let parser = LineParser::new(&buf);
        let err = ParseError {
            pos: 0,
            kind: ParseErrorKind::UnexpectedByte(b'@'),
        };
        let res = parser.parse();
        assert_eq!(res, Err(err), "failed: {:?}", buf);
    }
}
