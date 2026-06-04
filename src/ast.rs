use std::collections::HashMap;
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

impl Derivation {
    pub fn rewrite_paths(&mut self, map: &HashMap<String, String>) {
        // Handle outputs
        for out in &mut self.outputs {
            if let Some(nix_path) = map.get(&out.path) {
                out.path = nix_path.clone();
            }
        }

        // Handle input_drvs and potentially move them to input_srcs
        let mut new_input_drvs = Vec::new();
        let mut additional_srcs = Vec::new();

        for mut input_drv in self.input_drvs.drain(..) {
            if let Some(nix_path) = map.get(&input_drv.path) {
                if nix_path.ends_with(".drv") {
                    input_drv.path = nix_path.clone();
                    new_input_drvs.push(input_drv);
                } else {
                    // This was likely an FOD that we translated to a content path
                    additional_srcs.push(nix_path.clone());
                }
            } else {
                new_input_drvs.push(input_drv);
            }
        }
        self.input_drvs = new_input_drvs;
        self.input_srcs.extend(additional_srcs);
        self.input_srcs.sort();
        self.input_srcs.dedup();

        // Handle input_srcs
        for src in &mut self.input_srcs {
            if let Some(nix_path) = map.get(src) {
                *src = nix_path.clone();
            }
        }
        self.input_srcs.sort();
        self.input_srcs.dedup();

        // Handle builder
        if let Some(nix_path) = map.get(&self.builder) {
            self.builder = nix_path.clone();
        }

        // Handle args
        for arg in &mut self.args {
            for (old, new) in map {
                if arg.contains(old) {
                    *arg = arg.replace(old, new);
                }
            }
        }

        // Handle environment variables
        for env_var in &mut self.env {
            for (old, new) in map {
                if env_var.value.contains(old) {
                    env_var.value = env_var.value.replace(old, new);
                }
            }
        }
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
            write!(f, "({},{})", escape_string(&env.key), escape_string(&env.value))?;
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
