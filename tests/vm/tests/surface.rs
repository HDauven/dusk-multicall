//! The host functions the contract Wasm imports and the methods it exports.
use multicall_vm_tests::wasm;
use wasmparser::{ExternalKind, Parser, Payload, TypeRef};

#[test]
fn the_contract_imports_six_host_functions_and_exports_two_methods() {
    let bytes = wasm("dusk_multicall");
    let (mut imports, mut methods) = (vec![], vec![]);
    for payload in Parser::new(0).parse_all(&bytes) {
        match payload.unwrap() {
            Payload::ImportSection(reader) => {
                for import in reader {
                    let import = import.unwrap();
                    assert!(
                        matches!(import.ty, TypeRef::Func(_)),
                        "{}.{}",
                        import.module,
                        import.name
                    );
                    imports.push(format!("{}.{}", import.module, import.name));
                }
            }
            Payload::ExportSection(reader) => {
                for export in reader {
                    let export = export.unwrap();
                    if export.kind == ExternalKind::Func {
                        methods.push(export.name.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    imports.sort();
    methods.sort();
    assert_eq!(imports, ["env.c", "env.caller", "env.hd", "env.limit", "env.panic", "env.spent"]);
    assert_eq!(methods, ["aggregate", "version"]);
}
