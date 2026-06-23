use crate::ast::Derivation;
use crate::parser;
use std::collections::{HashMap, HashSet};
use std::fs;

pub struct DerivationGraph {
    pub derivations: HashMap<String, Derivation>,
    pub order: Vec<String>,
}

impl DerivationGraph {
    pub fn new() -> Self {
        Self {
            derivations: HashMap::new(),
            order: Vec::new(),
        }
    }

    pub fn load_recursive_multi(&mut self, root_paths: &[String]) -> Result<(), String> {
        let mut visited = HashSet::new();
        for path in root_paths {
            self.load_recursive_internal(path, &mut visited)?;
        }
        Ok(())
    }

    fn load_recursive_internal(
        &mut self,
        path: &str,
        visited: &mut HashSet<String>,
    ) -> Result<(), String> {
        if visited.contains(path) {
            return Ok(());
        }
        visited.insert(path.to_string());

        let content =
            fs::read_to_string(path).map_err(|e| format!("Failed to read {}: {}", path, e))?;

        let (_, drv) = parser::parse_derivation(&content)
            .map_err(|e| format!("Failed to parse {}: {:?}", path, e))?;

        for input_drv in &drv.input_drvs {
            self.load_recursive_internal(&input_drv.path, visited)?;
        }

        self.derivations.insert(path.to_string(), drv);
        self.order.push(path.to_string());

        Ok(())
    }
}
