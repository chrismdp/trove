use jsonschema::{Draft, JSONSchema};
use serde_json::json;

fn compile_default(s: &serde_json::Value) -> JSONSchema {
    JSONSchema::compile(s).expect("compile")
}
fn compile_2020(s: &serde_json::Value) -> JSONSchema {
    JSONSchema::options().with_draft(Draft::Draft202012).compile(s).expect("compile 2020")
}

fn main() {
    // allOf of two fragments; unevaluatedProperties:false at the composed level.
    // Correct 2020-12 behaviour: lat/lng ARE evaluated by the located branch,
    // so an object with name+lat+lng should PASS. A stray key should FAIL.
    let schema = json!({
        "allOf": [
            { "type":"object", "properties": { "name": {"type":"string"} }, "required":["name"] },
            { "type":"object", "properties": { "lat": {"type":"number"}, "lng": {"type":"number"} }, "required":["lat","lng"] }
        ],
        "unevaluatedProperties": false
    });

    let valid = json!({ "name":"x", "lat":1.0, "lng":2.0 });
    let stray = json!({ "name":"x", "lat":1.0, "lng":2.0, "bogus":true });

    for (label, compile) in [("DEFAULT draft", compile_default as fn(&serde_json::Value)->JSONSchema),
                             ("Draft2020-12", compile_2020)] {
        let c = compile(&schema);
        let v_ok = c.is_valid(&valid);
        let s_ok = c.is_valid(&stray);
        println!("[{label}]  valid-obj passes = {v_ok} (want true) | stray-key passes = {s_ok} (want false)");
        if v_ok && !s_ok { println!("   -> unevaluatedProperties WORKS correctly"); }
        else { println!("   -> unevaluatedProperties NOT honoured as 2020-12 (keyword likely ignored)"); }
    }
}
