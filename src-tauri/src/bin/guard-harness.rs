#[path = "../compat_guard.rs"]
mod compat_guard;
use compat_guard::{Adapter, Audit};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    time::{Duration, Instant},
};
struct Fixture {
    input: Value,
    explicit: bool,
    value: Option<u32>,
    calls: Vec<String>,
}
impl Adapter for Fixture {
    fn explicit(&self) -> bool {
        self.explicit
    }
    fn child_protected(&self) -> bool {
        self.input["fault"] != "child-not-protected"
    }
    fn call(&mut self, method: &str) -> Result<Option<u32>, String> {
        self.calls.push(method.into());
        eprintln!("CALL:{method}");
        std::io::stderr().flush().unwrap();
        let fault = self.input["fault"].as_str().unwrap_or("");
        if fault == "get-blocks-until-killed" && method == "get" {
            std::thread::sleep(Duration::from_secs(60));
        }
        if fault == "set-denied" && method == "set" {
            return Err("denied".into());
        }
        if fault == format!("{method}-fails") {
            return Err(format!("{method}-failed"));
        }
        Ok(match method {
            "presence" => Some(u32::from(self.input["rtss"].as_bool().unwrap())),
            "exists" => Some(u32::from(self.explicit)),
            "get" => self.value,
            "set" => {
                self.value = Some(0);
                None
            }
            "save" => {
                self.explicit = true;
                None
            }
            "readback" => {
                if self.explicit {
                    self.value
                } else {
                    None
                }
            }
            _ => None,
        })
    }
}
fn main() {
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text).unwrap();
    let input: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(input["target"], "suonelo.exe");
    let mut f = Fixture {
        explicit: input["explicit"].as_bool().unwrap(),
        value: input["value"].as_u64().map(|n| n as u32),
        input: input.clone(),
        calls: vec![],
    };
    let audit = Audit::open(std::path::Path::new(input["db"].as_str().unwrap())).unwrap();
    if input["prepare"] == true {
        println!("{}", json!({"prepared":true}));
        return;
    }
    if input["fault"] == "intent-fails" {
        audit.0.execute_batch("PRAGMA query_only=ON").unwrap()
    };
    let begin = Instant::now();
    let mut outcome = String::new();
    for n in 0..input["checks"].as_u64().unwrap_or(1) {
        outcome = compat_guard::check(
            &mut f,
            &audit,
            input["fault"] != "policy-off",
            &format!("op-{n}"),
            Duration::from_millis(75),
        );
    }
    let rows:Vec<Value>=audit.0.prepare("SELECT operation_id,sequence,phase,old_value,observed_value,error_code FROM guard_events ORDER BY event_id").unwrap().query_map([],|r|Ok(json!({"operation_id":r.get::<_,String>(0)?,"sequence":r.get::<_,i64>(1)?,"phase":r.get::<_,String>(2)?,"old_value":r.get::<_,Option<u32>>(3)?,"observed_value":r.get::<_,Option<u32>>(4)?,"error_code":r.get::<_,Option<String>>(5)?}))).unwrap().map(Result::unwrap).collect();
    println!(
        "{}",
        json!({"outcome":outcome,"calls":f.calls,"explicit":f.explicit,"value":f.value,"audit":rows,"guard_ms":begin.elapsed().as_secs_f64()*1000.0})
    );
}
