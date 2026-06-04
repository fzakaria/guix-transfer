use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derivation {
    pub outputs: Vec<Output>,
    pub input_drvs: Vec<InputDrv>,
    pub input_srcs: Vec<String>,
    pub system: String,
    pub builder: String,
    pub args: Vec<String>,
    pub env: Vec<EnvVar>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub name: String,
    pub path: String,
    pub hash_algo: String,
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDrv {
    pub path: String,
    pub outputs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
}

/// Name of a store object given its full path, i.e. the part after the
/// `<32-char-hash>-` prefix. `/gnu/store/abc…xyz-hello-2.12` → `hello-2.12`.
pub fn store_path_name(path: &str) -> &str {
    let base = path.rsplit('/').next().unwrap_or(path);
    // A store hash is 32 base32 chars followed by '-'. Be defensive: only skip
    // the prefix when it actually looks like `hash-`.
    if base.len() > 33 && base.as_bytes()[32] == b'-' {
        &base[33..]
    } else {
        base
    }
}

/// Derivation name for a `.drv` store path: the store name minus the `.drv`
/// suffix. `/gnu/store/…-hello-2.12.2.drv` → `hello-2.12.2`.
pub fn derivation_name(drv_path: &str) -> &str {
    store_path_name(drv_path)
        .strip_suffix(".drv")
        .unwrap_or_else(|| store_path_name(drv_path))
}

impl Derivation {
    /// Look up an env var value by key.
    pub fn env_get(&self, key: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|e| e.key == key)
            .map(|e| e.value.as_str())
    }
}

// Serialization
impl fmt::Display for Derivation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Derive(")?;

        // outputs
        write!(f, "[")?;
        let mut sorted_outputs = self.outputs.clone();
        sorted_outputs.sort_by(|a, b| a.name.cmp(&b.name));
        for (i, out) in sorted_outputs.iter().enumerate() {
            if i > 0 {
                write!(f, ",")?;
            }
            write!(
                f,
                "({},{},{},{})",
                escape_string(&out.name),
                escape_string(&out.path),
                escape_string(&out.hash_algo),
                escape_string(&out.hash)
            )?;
        }
        write!(f, "],")?;

        // input_drvs
        write!(f, "[")?;
        let mut sorted_drvs = self.input_drvs.clone();
        sorted_drvs.sort_by(|a, b| a.path.cmp(&b.path));
        for (i, drv) in sorted_drvs.iter().enumerate() {
            if i > 0 {
                write!(f, ",")?;
            }
            write!(f, "({},[", escape_string(&drv.path))?;
            let mut sorted_outs = drv.outputs.clone();
            sorted_outs.sort();
            for (j, out) in sorted_outs.iter().enumerate() {
                if j > 0 {
                    write!(f, ",")?;
                }
                write!(f, "{}", escape_string(out))?;
            }
            write!(f, "])")?;
        }
        write!(f, "],")?;

        // input_srcs
        write!(f, "[")?;
        let mut sorted_srcs = self.input_srcs.clone();
        sorted_srcs.sort();
        for (i, src) in sorted_srcs.iter().enumerate() {
            if i > 0 {
                write!(f, ",")?;
            }
            write!(f, "{}", escape_string(src))?;
        }
        write!(f, "],")?;

        // system
        write!(f, "{},", escape_string(&self.system))?;

        // builder
        write!(f, "{},", escape_string(&self.builder))?;

        // args
        write!(f, "[")?;
        for (i, arg) in self.args.iter().enumerate() {
            if i > 0 {
                write!(f, ",")?;
            }
            write!(f, "{}", escape_string(arg))?;
        }
        write!(f, "],")?;

        // env
        write!(f, "[")?;
        let mut sorted_env = self.env.clone();
        sorted_env.sort_by(|a, b| a.key.cmp(&b.key));
        for (i, env) in sorted_env.iter().enumerate() {
            if i > 0 {
                write!(f, ",")?;
            }
            write!(
                f,
                "({},{})",
                escape_string(&env.key),
                escape_string(&env.value)
            )?;
        }
        write!(f, "])")?;

        Ok(())
    }
}

pub fn escape_string(s: &str) -> String {
    let mut escaped = String::with_capacity(s.len() + 2);
    escaped.push('"');
    for c in s.chars() {
        match c {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(c),
        }
    }
    escaped.push('"');
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Derivation {
        Derivation {
            outputs: vec![Output {
                name: "out".into(),
                path: "/gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-minimal".into(),
                hash_algo: String::new(),
                hash: String::new(),
            }],
            input_drvs: vec![],
            input_srcs: vec![],
            system: "x86_64-linux".into(),
            builder: "/bin/sh".into(),
            args: vec!["-c".into(), "echo hi > $out".into()],
            env: vec![EnvVar {
                key: "out".into(),
                value: "/gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-minimal".into(),
            }],
        }
    }

    #[test]
    fn name_extraction() {
        assert_eq!(
            store_path_name("/gnu/store/w9krgvil6919s2ghqgx443zb9krx75s6-hello-2.12.2"),
            "hello-2.12.2"
        );
        assert_eq!(
            derivation_name("/gnu/store/w9krgvil6919s2ghqgx443zb9krx75s6-hello-2.12.2.drv"),
            "hello-2.12.2"
        );
        // Names containing dashes are preserved in full (regression for the old
        // `split('-').nth(1)` bug).
        assert_eq!(
            store_path_name("/gnu/store/cvy2j7mr0q0vwv3dnhhqkaa548kk4q88-hello-source"),
            "hello-source"
        );
    }

    #[test]
    fn aterm_roundtrips_through_parser() {
        let d = sample();
        let text = format!("{d}");
        let (rest, parsed) = crate::parser::parse_derivation(&text).unwrap();
        assert!(rest.is_empty());
        assert_eq!(parsed, d);
    }

    #[test]
    fn escape_roundtrip() {
        assert_eq!(escape_string("a\"b\\c\nd"), "\"a\\\"b\\\\c\\nd\"");
    }
}
