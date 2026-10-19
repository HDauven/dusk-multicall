//! The data driver against the golden vectors in `tests/vectors.json`, which
//! `tests/driver.test.mjs` checks the same way against the driver's Wasm.
#![cfg(feature = "data-driver")]
use dusk_data_driver::{ConvertibleContract, Error};
use dusk_multicall::driver::Driver;
use dusk_multicall::{Aggregate, Call, CallError, ContractId};
use serde_json::{Value, json};

fn vectors() -> Vec<Value> {
    serde_json::from_str(include_str!("../../tests/vectors.json")).unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn encode(fn_name: &str, json: &Value) -> Result<Vec<u8>, Error> {
    Driver.encode_input_fn(fn_name, &json.to_string())
}

#[test]
fn the_driver_reproduces_every_vector() {
    for v in vectors() {
        let (name, rkyv) = (v["fn"].as_str().unwrap(), v["rkyv"].as_str().unwrap());
        let bytes: Vec<u8> = (0..rkyv.len() / 2)
            .map(|i| u8::from_str_radix(&rkyv[2 * i..2 * i + 2], 16).unwrap())
            .collect();
        if v["kind"] == "input" {
            assert_eq!(hex(&encode(name, &v["json"]).unwrap()), rkyv, "{name}");
            assert_eq!(Driver.decode_input_fn(name, &bytes).unwrap(), v["json"], "{name}");
        } else {
            assert_eq!(Driver.decode_output_fn(name, &bytes).unwrap(), v["json"], "{name}");
        }
    }
}

#[test]
fn the_vectors_are_the_archives_of_the_rust_types() {
    let mut transfer = [0; 32];
    transfer[0] = 1;
    let calls = vec![
        Call {
            contract: ContractId::from_bytes(transfer),
            fn_name: "account".into(),
            fn_args: vec![0xde, 0xad, 0xbe, 0xef],
            gas_limit: 0,
        },
        Call {
            contract: ContractId::from_bytes([0xaa; 32]),
            fn_name: "a_name_longer_than_eight_bytes".into(),
            fn_args: vec![],
            gas_limit: 1_000_000,
        },
    ];
    let aggregate = Aggregate {
        height: 123_456,
        results: vec![
            Ok(vec![1, 2]),
            Err(CallError::Panic("boom".into())),
            Err(CallError::OutOfGas),
            Err(CallError::DoesNotExist),
            Err(CallError::Unknown),
            Ok(vec![]),
        ],
    };
    let v = vectors();
    assert_eq!(hex(&rkyv::to_bytes::<_, 1024>(&calls).unwrap()), v[0]["rkyv"]);
    assert_eq!(hex(&rkyv::to_bytes::<_, 1024>(&aggregate).unwrap()), v[2]["rkyv"]);
}

#[test]
fn gas_limits_may_be_numbers_or_decimal_strings() {
    let call = |gas_limit: Value| json!([{ "contract": "aa".repeat(32), "fn_name": "f", "fn_args": "00", "gas_limit": gas_limit }]);
    let string = encode("aggregate", &call(json!("18446744073709551615"))).unwrap();
    assert_eq!(encode("aggregate", &call(json!(u64::MAX))).unwrap(), string);
    let decoded = Driver.decode_input_fn("aggregate", &string).unwrap();
    assert_eq!(decoded[0]["gas_limit"], "18446744073709551615");
}

#[test]
fn malformed_json_is_refused() {
    let good =
        json!({ "contract": "aa".repeat(32), "fn_name": "f", "fn_args": "", "gas_limit": "0" });
    assert!(encode("aggregate", &json!([good])).is_ok());
    let with = |key: &str, value: Value| {
        let mut call = good.clone();
        call[key] = value;
        json!([call])
    };
    let mut missing = good.clone();
    missing.as_object_mut().unwrap().remove("gas_limit");
    for (case, json) in [
        ("missing field", json!([missing])),
        ("unknown field", with("extra", json!(1))),
        ("short contract ID", with("contract", json!("aa".repeat(31)))),
        ("not hex", with("fn_args", json!("zz"))),
        ("odd hex", with("fn_args", json!("abc"))),
        ("negative gas", with("gas_limit", json!(-1))),
        ("gas over u64", with("gas_limit", json!("18446744073709551616"))),
        // The field an earlier draft of the ABI had.
        ("allow_failure", with("allow_failure", json!(true))),
        ("not a list", good.clone()),
    ] {
        assert!(encode("aggregate", &json).is_err(), "{case}");
    }
}

#[test]
fn only_the_two_exports_are_known() {
    assert!(matches!(encode("transfer", &json!(null)), Err(Error::Unsupported(_))));
    assert!(matches!(Driver.decode_output_fn("init", &[]), Err(Error::Unsupported(_))));
    assert!(matches!(Driver.decode_event("anything", &[]), Err(Error::Unsupported(_))));
    let schema: Value = serde_json::from_str(&Driver.get_schema()).unwrap();
    println!("{}", serde_json::to_string_pretty(&schema).unwrap());
    let functions: Vec<_> = schema["functions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["name"].as_str().unwrap(),
                f["input"].as_str().unwrap(),
                f["output"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(functions, [("aggregate", "Vec < Call >", "Aggregate"), ("version", "()", "u32")]);
    assert_eq!(schema["events"], json!([]));
}
