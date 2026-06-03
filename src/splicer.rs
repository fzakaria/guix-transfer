use crate::graph::DerivationGraph;
use crate::ast::EnvVar;
use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;

pub struct Splicer {
    pub guix_to_nix_map: HashMap<String, String>,
    pub boundary_regex: Regex,
    pub nix_stdenv_drv: String,
}

impl Splicer {
    pub fn new(nix_stdenv_drv: String) -> Self {
        Self {
            guix_to_nix_map: HashMap::new(),
            boundary_regex: Regex::new(r"gcc-bootstrap|glibc-bootstrap|bootstrap-binaries").unwrap(),
            nix_stdenv_drv,
        }
    }

    pub fn run(&mut self, graph: &DerivationGraph) -> Result<String, String> {
        let mut last_nix_drv = String::new();

        for drv_path in &graph.order {
            if self.boundary_regex.is_match(drv_path) {
                self.guix_to_nix_map
                    .insert(drv_path.clone(), self.nix_stdenv_drv.clone());
                last_nix_drv = self.nix_stdenv_drv.clone();
                continue;
            }

            let mut drv = graph.derivations[drv_path].clone();

            // 1. Handle Builtin Builders (Guix builtin:download -> Nix builtin:fetchurl)
            if drv.builder == "builtin:download" {
                drv.builder = "builtin:fetchurl".to_string();
                drv.system = "builtin".to_string();

                // Inject mandatory Nix environment variables
                let name = drv_path.split('-').nth(1).unwrap_or("source").replace(".drv", "");
                drv.env.push(EnvVar { key: "name".to_string(), value: name });

                if !drv.outputs.is_empty() {
                    let hash = drv.outputs[0].hash.clone();
                    let algo = drv.outputs[0].hash_algo.clone();
                    drv.env.push(EnvVar { key: "outputHash".to_string(), value: hash });
                    drv.env.push(EnvVar { key: "outputHashAlgo".to_string(), value: algo });
                    drv.env.push(EnvVar { key: "outputHashMode".to_string(), value: "flat".to_string() });
                }

                for env_var in &mut drv.env {
                    if env_var.key == "url" {
                        let mut urls = env_var
                            .value
                            .replace("(", "")
                            .replace(")", "")
                            .replace("\"", "");
                        
                        // Expand mirror://gnu/ to a real URL
                        urls = urls.replace("mirror://gnu/", "https://ftp.gnu.org/gnu/");
                        
                        // builtin:fetchurl strictly requires a single 'url' string.
                        // If Guix provided a list, we just take the first one.
                        let first_url = urls.split_whitespace().next().unwrap_or("").to_string();
                        
                        env_var.value = first_url;
                    }
                }
            }

            // Check if it's an FOD
            let is_fod = drv.outputs.iter().any(|o| !o.hash_algo.is_empty());
            if is_fod && drv.builder != "builtin:fetchurl" {
                let mut all_exists = true;
                for out in &drv.outputs {
                    if Path::new(&out.path).exists() {
                        let nix_path = self.add_path_to_nix_store(&out.path)?;
                        self.guix_to_nix_map.insert(out.path.clone(), nix_path.clone());
                    } else {
                        all_exists = false;
                    }
                }
                if all_exists {
                    let primary_nix_out = self.guix_to_nix_map[&drv.outputs[0].path].clone();
                    self.guix_to_nix_map.insert(drv_path.clone(), primary_nix_out.clone());
                    last_nix_drv = primary_nix_out;
                    continue;
                }
            }

            // 2. Rewrite input scripts and sources
            let mut srcs_to_add = drv.input_srcs.clone();
            for src in &mut srcs_to_add {
                if src.starts_with("/gnu/store") {
                    if let Some(nix_src) = self.guix_to_nix_map.get(src) {
                        *src = nix_src.clone();
                    } else {
                        let metadata = fs::metadata(&*src)
                            .map_err(|e| format!("Failed to get metadata for {}: {}", src, e))?;

                        let nix_src = if metadata.is_dir() {
                            self.add_path_to_nix_store(&*src)?
                        } else {
                            if self.is_text_file(src)? {
                                let content = fs::read_to_string(&*src)
                                    .map_err(|e| format!("Failed to read script {}: {}", src, e))?;

                                let mut new_content = content;
                                for (old, new) in &self.guix_to_nix_map {
                                    new_content = new_content.replace(old, new);
                                }
                                self.add_to_nix_store(&new_content, &*src)?
                            } else {
                                self.add_path_to_nix_store(&*src)?
                            }
                        };
                        self.guix_to_nix_map.insert(src.clone(), nix_src.clone());
                        *src = nix_src;
                    }
                }
            }
            drv.input_srcs = srcs_to_add;

            // 3. Rewrite derivation paths (dependencies)
            drv.rewrite_paths(&self.guix_to_nix_map);

            // 4. Iteratively fix output paths and add to Nix store
            for out in &mut drv.outputs {
                if out.path.starts_with("/gnu/store") {
                    out.path = out.path.replace("/gnu/store", "/nix/store");
                }
            }
            for env_var in &mut drv.env {
                if env_var.value.starts_with("/gnu/store") {
                    env_var.value = env_var.value.replace("/gnu/store", "/nix/store");
                }
                for (old, new) in &self.guix_to_nix_map {
                    if env_var.value.contains(old) {
                        env_var.value = env_var.value.replace(old, new);
                    }
                }
            }

            let mut drv_content = format!("{}", drv);
            let mut attempt = 0;
            let nix_drv = loop {
                attempt += 1;
                if attempt > 25 {
                    return Err(format!("Failed to converge on output paths for {}", drv_path));
                }

                match self.add_to_nix_store(&drv_content, drv_path) {
                    Ok(path) => break path,
                    Err(e) if e.contains("should be '") => {
                        let re_var = Regex::new(r"incorrect environment variable '([^']+)'").unwrap();
                        let re_incorrect_out = Regex::new(r"incorrect output '([^']+)'").unwrap();
                        let re_path = Regex::new(r"should be '(/nix/store/[^']+)'").unwrap();

                        let expected = re_path.captures(&e).and_then(|c| c.get(1)).map(|m| m.as_str().to_string());

                        if let Some(expected_path) = expected {
                            if let Some(var_caps) = re_var.captures(&e) {
                                let var = var_caps.get(1).unwrap().as_str();
                                for out in &mut drv.outputs {
                                    if out.name == var { out.path = expected_path.clone(); }
                                }
                                for env_var in &mut drv.env {
                                    if env_var.key == var { env_var.value = expected_path.clone(); }
                                }
                            } else if let Some(out_caps) = re_incorrect_out.captures(&e) {
                                let actual_path = out_caps.get(1).unwrap().as_str();
                                for out in &mut drv.outputs {
                                    if out.path == actual_path {
                                        let name = out.name.clone();
                                        out.path = expected_path.clone();
                                        for env_var in &mut drv.env {
                                            if env_var.key == name { env_var.value = expected_path.clone(); }
                                        }
                                    }
                                }
                            }
                            drv_content = format!("{}", drv);
                            continue;
                        }
                        return Err(e);
                    }
                    Err(e) => return Err(e),
                }
            };

            self.guix_to_nix_map.insert(drv_path.clone(), nix_drv.clone());
            for (out_idx, out) in drv.outputs.iter().enumerate() {
                let guix_out = graph.derivations[drv_path].outputs[out_idx].path.clone();
                self.guix_to_nix_map.insert(guix_out, out.path.clone());
            }

            last_nix_drv = nix_drv;
        }

        Ok(last_nix_drv)
    }

    fn is_text_file(&self, path: &str) -> Result<bool, String> {
        let mut f = fs::File::open(path).map_err(|e| e.to_string())?;
        use std::io::Read;
        let mut buffer = [0u8; 1024];
        let n = f.read(&mut buffer).map_err(|e| e.to_string())?;
        Ok(!buffer[..n].contains(&0))
    }

    fn add_path_to_nix_store(&self, path: &str) -> Result<String, String> {
        let output = Command::new("nix-store")
            .arg("--add")
            .arg(path)
            .output()
            .map_err(|e| format!("Failed to run nix-store --add: {}", e))?;

        if !output.status.success() {
            return Err(format!("nix-store --add failed for {}: {}", path, String::from_utf8_lossy(&output.stderr)));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn add_to_nix_store(&self, content: &str, original_path: &str) -> Result<String, String> {
        let name = original_path.split('/').last().unwrap_or("artifact");
        let tmp_path = format!("/tmp/{}", name);
        fs::write(&tmp_path, content)
            .map_err(|e| format!("Failed to write temp file {}: {}", tmp_path, e))?;

        let output = Command::new("nix-store")
            .arg("--add")
            .arg(&tmp_path)
            .output()
            .map_err(|e| format!("Failed to run nix-store --add: {}", e))?;

        if !output.status.success() {
            fs::remove_file(&tmp_path).ok();
            return Err(format!("nix-store --add failed: {}", String::from_utf8_lossy(&output.stderr)));
        }

        let nix_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        fs::remove_file(&tmp_path).ok();
        Ok(nix_path)
    }
}
