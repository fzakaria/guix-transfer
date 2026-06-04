use crate::ast::*;
use nom::{
    IResult, Parser,
    bytes::complete::tag,
    character::complete::char,
    combinator::map,
    multi::separated_list0,
    sequence::{delimited, preceded},
};

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
    Err(nom::Err::Error(nom::error::Error::new(
        input,
        nom::error::ErrorKind::Tag,
    )))
}

fn parse_list<'a, T, F>(mut inner: F) -> impl FnMut(&'a str) -> IResult<&'a str, Vec<T>>
where
    F: FnMut(&'a str) -> IResult<&'a str, T>,
{
    move |input| {
        delimited(char('['), separated_list0(char(','), &mut inner), char(']')).parse(input)
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
    )
    .parse(input)
}

fn parse_input_drv(input: &str) -> IResult<&str, InputDrv> {
    delimited(
        char('('),
        map(
            (parse_string, preceded(char(','), parse_list(parse_string))),
            |(path, outputs)| InputDrv { path, outputs },
        ),
        char(')'),
    )
    .parse(input)
}

fn parse_env_var(input: &str) -> IResult<&str, EnvVar> {
    delimited(
        char('('),
        map(
            (parse_string, preceded(char(','), parse_string)),
            |(key, value)| EnvVar { key, value },
        ),
        char(')'),
    )
    .parse(input)
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
    )
        .parse(input)?;
    let (input, _) = char(')')(input)?;
    Ok((
        input,
        Derivation {
            outputs,
            input_drvs,
            input_srcs,
            system,
            builder,
            args,
            env,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_derivation() {
        // Real output of examples/1-minimal.scm.
        let s = r#"Derive([("out","/gnu/store/280xcws0xz6c1gx6721jyj2d8j0ly9gz-minimal","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo 'Success' > $out"],[("PATH","/bin"),("out","/gnu/store/280xcws0xz6c1gx6721jyj2d8j0ly9gz-minimal")])"#;
        let (rest, d) = parse_derivation(s).unwrap();
        assert!(rest.is_empty());
        assert_eq!(d.outputs.len(), 1);
        assert_eq!(d.outputs[0].name, "out");
        assert_eq!(d.outputs[0].hash_algo, "");
        assert_eq!(d.builder, "/bin/sh");
        assert_eq!(d.args, vec!["-c", "echo 'Success' > $out"]);
        assert_eq!(d.env_get("PATH"), Some("/bin"));
        assert!(d.input_drvs.is_empty());
        assert!(d.input_srcs.is_empty());
    }

    #[test]
    fn parses_fixed_output_and_inputs() {
        // Real output of examples/2-fod.scm — note the nested-quote url value.
        let s = r#"Derive([("out","/gnu/store/4yvc3d9azxnj22kmz8g5iqik88j8gpc0-hello-source","sha256","cf04af86dc085268c5f4470fbae49b18afbc221b78096aab842d934a76bad0ab")],[],[],"x86_64-linux","builtin:download",[],[("out","/gnu/store/4yvc3d9azxnj22kmz8g5iqik88j8gpc0-hello-source"),("url","(\"https://ftp.gnu.org/gnu/hello/hello-2.12.tar.gz\")")])"#;
        let (rest, d) = parse_derivation(s).unwrap();
        assert!(rest.is_empty());
        assert_eq!(d.outputs[0].hash_algo, "sha256");
        assert_eq!(d.builder, "builtin:download");
        // The url env carries an escaped Scheme list, unescaped by the parser.
        assert_eq!(
            d.env_get("url"),
            Some("(\"https://ftp.gnu.org/gnu/hello/hello-2.12.tar.gz\")")
        );
    }

    #[test]
    fn parses_input_drvs_and_srcs() {
        let s = r#"Derive([("out","/gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a","","")],[("/gnu/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b.drv",["out","lib"])],["/gnu/store/cccccccccccccccccccccccccccccccc-s.sh"],"x86_64-linux","/bin/sh",[],[])"#;
        let (_, d) = parse_derivation(s).unwrap();
        assert_eq!(d.input_drvs.len(), 1);
        assert_eq!(
            d.input_drvs[0].path,
            "/gnu/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b.drv"
        );
        assert_eq!(d.input_drvs[0].outputs, vec!["out", "lib"]);
        assert_eq!(
            d.input_srcs,
            vec!["/gnu/store/cccccccccccccccccccccccccccccccc-s.sh"]
        );
    }

    #[test]
    fn parse_string_handles_escapes() {
        let (rest, s) = parse_string(r#""a\"b\\c\n\t""#).unwrap();
        assert!(rest.is_empty());
        assert_eq!(s, "a\"b\\c\n\t");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_derivation("not a derivation").is_err());
    }
}
