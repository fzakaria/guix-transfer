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

    pub fn compute_layers(&self) -> Vec<Vec<String>> {
        let mut in_degree: HashMap<&String, usize> = HashMap::new();
        let mut reverse_edges: HashMap<&String, Vec<&String>> = HashMap::new();

        for (path, drv) in &self.derivations {
            in_degree.entry(path).or_insert(0);
            for input in &drv.input_drvs {
                reverse_edges.entry(&input.path).or_default().push(path);
                *in_degree.entry(path).or_insert(0) += 1;
            }
        }

        let mut queue = std::collections::VecDeque::new();
        for (node, &deg) in &in_degree {
            if deg == 0 {
                queue.push_back(*node);
            }
        }

        let mut layers = Vec::new();
        while !queue.is_empty() {
            let mut next_queue = std::collections::VecDeque::new();
            let mut layer = Vec::new();
            for node in queue {
                layer.push(node.clone());
                if let Some(neighbors) = reverse_edges.get(node) {
                    for &neighbor in neighbors {
                        let deg = in_degree.get_mut(neighbor).unwrap();
                        *deg -= 1;
                        if *deg == 0 {
                            next_queue.push_back(neighbor);
                        }
                    }
                }
            }
            layers.push(layer);
            queue = next_queue;
        }
        layers
    }
}
