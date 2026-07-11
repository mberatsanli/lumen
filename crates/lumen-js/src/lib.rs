//! lumen-js: a small, hand-written JavaScript engine for the Lumen
//! browser — lexer, recursive-descent parser and a tree-walking
//! interpreter over a practical language subset (closures, arrays,
//! objects, arrow functions, for-of, timers).
//!
//! The page is reached through the [`Host`] trait: the interpreter knows
//! element *handles*, never engine types, so the browser crate stays the
//! only place that touches the DOM.

pub mod ast;
pub mod interp;
pub mod lexer;
pub mod parser;

pub use interp::{DomNode, Host, Runtime, Value, to_display, to_number, truthy};
pub use parser::parse_program;

#[cfg(test)]
mod tests {
    use super::*;

    /// A host that records console output and serves a fake single-node
    /// DOM.
    #[derive(Default)]
    struct TestHost {
        log: Vec<String>,
        text: std::collections::HashMap<DomNode, String>,
        value: std::collections::HashMap<DomNode, String>,
    }

    impl Host for TestHost {
        fn console_log(&mut self, message: &str) {
            self.log.push(message.to_string());
        }
        fn get_element_by_id(&mut self, id: &str) -> Option<DomNode> {
            (id == "box").then_some(7)
        }
        fn query_selector_all(&mut self, _selector: &str) -> Vec<DomNode> {
            vec![7]
        }
        fn get_text(&mut self, node: DomNode) -> String {
            self.text.get(&node).cloned().unwrap_or_default()
        }
        fn set_text(&mut self, node: DomNode, text: &str) {
            self.text.insert(node, text.to_string());
        }
        fn get_value(&mut self, node: DomNode) -> String {
            self.value.get(&node).cloned().unwrap_or_default()
        }
        fn set_value(&mut self, node: DomNode, value: &str) {
            self.value.insert(node, value.to_string());
        }
        fn get_attribute(&mut self, _node: DomNode, name: &str) -> Option<String> {
            (name == "id").then(|| "box".to_string())
        }
        fn set_attribute(&mut self, _node: DomNode, _name: &str, _value: &str) {}
        fn set_style(&mut self, _node: DomNode, _property: &str, _value: &str) {}
        fn random(&mut self) -> f64 {
            0.5
        }
    }

    fn logs(source: &str) -> Vec<String> {
        let mut runtime = Runtime::new();
        let mut host = TestHost::default();
        runtime.run(source, &mut host).unwrap();
        host.log
    }

    #[test]
    fn arithmetic_and_strings() {
        assert_eq!(
            logs("console.log(1 + 2 * 3, 'a' + 1, 10 % 3, 7 / 2)"),
            vec!["7 a1 1 3.5"]
        );
    }

    #[test]
    fn variables_and_reassignment() {
        assert_eq!(
            logs("let x = 1; x += 4; x *= 2; console.log(x)"),
            vec!["10"]
        );
    }

    #[test]
    fn functions_and_closures() {
        assert_eq!(
            logs(
                "function makeCounter() { let n = 0; return function() { n++; return n; }; }
                 const next = makeCounter();
                 next(); next();
                 console.log(next());"
            ),
            vec!["3"]
        );
    }

    #[test]
    fn arrow_functions_and_array_methods() {
        assert_eq!(
            logs(
                "const xs = [1, 2, 3, 4];
                 const doubled = xs.map(x => x * 2).filter(x => x > 4);
                 console.log(doubled.join('-'), xs.length);"
            ),
            vec!["6-8 4"]
        );
    }

    #[test]
    fn control_flow() {
        assert_eq!(
            logs(
                "let total = 0;
                 for (let i = 0; i < 10; i++) { if (i % 2 === 0) continue; total += i; }
                 let n = 0; while (true) { n++; if (n === 3) break; }
                 console.log(total, n);"
            ),
            vec!["25 3"]
        );
    }

    #[test]
    fn for_of_and_objects() {
        assert_eq!(
            logs(
                "const user = { name: 'ada', age: 36 };
                 let out = '';
                 for (const c of 'ab') { out += c; }
                 console.log(user.name, user['age'], out, JSON.stringify({b:1,a:[true]}));"
            ),
            vec![r#"ada 36 ab {"a":[true],"b":1}"#]
        );
    }

    #[test]
    fn equality_and_logic() {
        assert_eq!(
            logs("console.log(1 == '1', 1 === '1', null ?? 'x', 0 || 'y', 2 && 'z')"),
            vec!["true false x y z"]
        );
    }

    #[test]
    fn string_methods() {
        assert_eq!(
            logs("console.log(' Hi '.trim().toUpperCase(), 'abcdef'.slice(1, -1), 'a,b'.split(','.charAt(0)).join('|'))"),
            vec!["HI bcde a|b"]
        );
    }

    #[test]
    fn dom_read_write_and_events() {
        let mut runtime = Runtime::new();
        let mut host = TestHost::default();
        runtime
            .run(
                "const box = document.getElementById('box');
                 box.textContent = 'hello';
                 let clicks = 0;
                 box.addEventListener('click', function(event) {
                     clicks++;
                     box.textContent = 'clicked ' + clicks + ' on ' + event.target.id;
                 });",
                &mut host,
            )
            .unwrap();
        assert_eq!(host.text.get(&7).map(String::as_str), Some("hello"));
        assert!(runtime.has_listener(7, "click"));
        runtime.dispatch_event(7, "click", &mut host);
        runtime.dispatch_event(7, "click", &mut host);
        assert_eq!(
            host.text.get(&7).map(String::as_str),
            Some("clicked 2 on box")
        );
    }

    #[test]
    fn timers_fire_in_order() {
        let mut runtime = Runtime::new();
        let mut host = TestHost::default();
        runtime
            .run(
                "setTimeout(function() { console.log('later'); }, 100);
                 console.log('now');",
                &mut host,
            )
            .unwrap();
        assert!(runtime.has_timers());
        assert!(!runtime.run_timers(50.0, &mut host));
        assert!(runtime.run_timers(150.0, &mut host));
        assert_eq!(host.log, vec!["now", "later"]);
        assert!(!runtime.has_timers());
    }

    #[test]
    fn errors_carry_messages() {
        let mut runtime = Runtime::new();
        let mut host = TestHost::default();
        let error = runtime.run("missing()", &mut host).unwrap_err();
        assert!(error.contains("missing"), "{error}");
        let error = runtime.run("let x = ;", &mut host).unwrap_err();
        assert!(error.contains("line 1"), "{error}");
    }

    #[test]
    fn recursion_is_depth_limited() {
        let mut runtime = Runtime::new();
        let mut host = TestHost::default();
        let error = runtime
            .run("function f() { return f(); } f();", &mut host)
            .unwrap_err();
        assert!(error.contains("stack"), "{error}");
    }
}
