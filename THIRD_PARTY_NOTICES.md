# Third-Party Notices

AGY Hyper vendors the following open-source libraries locally inside `plugin/sidecars/hyper_deck/vendor/` so the Antigravity Sidecar Cockpit runs **100% offline with zero external CDN requests**:

## Apache ECharts (`echarts@6.1.0`)
- **File**: `plugin/sidecars/hyper_deck/vendor/echarts-core.min.js` (tree-shaken custom bundle)
- **Project**: https://echarts.apache.org/
- **License**: Apache License 2.0 (https://www.apache.org/licenses/LICENSE-2.0)
- **Copyright**: Copyright © The Apache Software Foundation

## i18next (`i18next@27.2.2`)
- **File**: `plugin/sidecars/hyper_deck/vendor/i18next.min.js`
- **Project**: https://www.i18next.com/
- **License**: MIT License
- **Copyright**: Copyright © i18next contributors

## Rust Crates
All Rust dependencies compiled into the single `agy-hyper` binary are declared in `Cargo.toml` and crate manifests (`crates/*/Cargo.toml`) under permissive MIT / Apache-2.0 licenses (including `tree-sitter`, `gix`, `grep-searcher`, `ignore`, `blake3`, `tokio`, `notify`, `similar`, `clap`, `serde`).
