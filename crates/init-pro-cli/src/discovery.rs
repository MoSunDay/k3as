//! API discovery wiring (TODO **T1.1**, S7 skeleton).
//!
//! Builds the [`SchemaRegistry`] served by `init-pro server` (core/v1
//! pass-through + the `init-pro.io/v1` group) and the discovery document
//! bodies for `/api` and `/apis`. The actual HTTP serving is **T1.2**; this
//! module hands T1.2 a byte-correct registry today and is exercised at startup.

use init_pro_api::{api_group_list, core_api_versions, initpro, SchemaRegistry};

/// The served schema registry: core/v1 native types + init-pro.io/v1 CRDs.
pub fn served_schema() -> SchemaRegistry {
    let mut reg = SchemaRegistry::with_core_v1();
    initpro::register(&mut reg);
    reg
}

/// One-line summary of served API groups (for startup logging).
pub fn served_groups_summary(reg: &SchemaRegistry, server_addr: &str) -> String {
    let core = core_api_versions(reg, server_addr);
    let groups = api_group_list(reg);
    let mut names: Vec<&str> = groups.groups.iter().map(|g| g.name.as_str()).collect();
    names.insert(0, "(core)");
    format!(
        "serving core versions [{}] + {} group(s): {}",
        core.versions.join(", "),
        groups.groups.len(),
        names.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn served_schema_has_core_and_initpro() {
        let reg = served_schema();
        assert!(reg.len() >= 8); // 7 core/v1 + LuaRouter
        let summary = served_groups_summary(&reg, "127.0.0.1:6443");
        assert!(summary.contains("(core)"));
        assert!(summary.contains("init-pro.io"));
        assert!(summary.contains("v1"));
    }
}
