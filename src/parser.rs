use nom::{
    bytes::complete::tag,
    character::complete::char,
    combinator::map,
    multi::separated_list0,
    sequence::{delimited, preceded},
    IResult, Parser,
};
use crate::ast::*;

fn parse_string(input: &str) -> IResult<&str, String> {
    let (input, _) = char('"')(input)?;
    let mut s = String::new();
    let mut chars = input.chars();
    let mut consumed = 0;
    while let Some(c) = chars.next() {
        consumed += c.len_utf8();
        if c == '"' {
            return Ok((&input[consumed..], s));
        }
        if c == '\\' {
            if let Some(next) = chars.next() {
                consumed += next.len_utf8();
                match next {
                    '\\' => s.push('\\'),
                    '"' => s.push('"'),
                    'n' => s.push('\n'),
                    'r' => s.push('\r'),
                    't' => s.push('\t'),
                    _ => {
                        s.push('\\');
                        s.push(next);
                    }
                }
            }
        } else {
            s.push(c);
        }
    }
    Err(nom::Err::Error(nom::error::Error::new(input, nom::error::ErrorKind::Tag)))
}

fn parse_list<'a, T, F>(mut inner: F) -> impl FnMut(&'a str) -> IResult<&'a str, Vec<T>>
where
    F: FnMut(&'a str) -> IResult<&'a str, T>,
{
    move |input| {
        delimited(
            char('['),
            separated_list0(char(','), |i| inner(i)),
            char(']'),
        ).parse(input)
    }
}

fn parse_output(input: &str) -> IResult<&str, Output> {
    delimited(
        char('('),
        map(
            (
                parse_string,
                preceded(char(','), parse_string),
                preceded(char(','), parse_string),
                preceded(char(','), parse_string),
            ),
            |(name, path, hash_algo, hash)| Output {
                name,
                path,
                hash_algo,
                hash,
            },
        ),
        char(')'),
    ).parse(input)
}

fn parse_input_drv(input: &str) -> IResult<&str, InputDrv> {
    delimited(
        char('('),
        map(
            (
                parse_string,
                preceded(char(','), parse_list(parse_string)),
            ),
            |(path, outputs)| InputDrv { path, outputs },
        ),
        char(')'),
    ).parse(input)
}

fn parse_env_var(input: &str) -> IResult<&str, EnvVar> {
    delimited(
        char('('),
        map(
            (
                parse_string,
                preceded(char(','), parse_string),
            ),
            |(key, value)| EnvVar { key, value },
        ),
        char(')'),
    ).parse(input)
}

pub fn parse_derivation(input: &str) -> IResult<&str, Derivation> {
    let (input, _) = tag("Derive(")(input)?;
    let (input, (outputs, input_drvs, input_srcs, system, builder, args, env)) = (
        parse_list(parse_output),
        preceded(char(','), parse_list(parse_input_drv)),
        preceded(char(','), parse_list(parse_string)),
        preceded(char(','), parse_string),
        preceded(char(','), parse_string),
        preceded(char(','), parse_list(parse_string)),
        preceded(char(','), parse_list(parse_env_var)),
    ).parse(input)?;
    let (input, _) = char(')')(input)?;
    Ok((input, Derivation {
        outputs,
        input_drvs,
        input_srcs,
        system,
        builder,
        args,
        env,
    }))
}
