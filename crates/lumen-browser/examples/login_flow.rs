//! Manual end-to-end probe: a real login flow (POST + session cookie).
//!
//! Drives the practice site the-internet.herokuapp.com (built for
//! automation testing) headlessly:
//!
//! ```bash
//! cargo run -p lumen-browser --example login_flow
//! ```

use lumen_browser::Session;
use lumen_engine::Size;
use lumen_platform::DefaultLoader;

fn main() {
    let mut session = Session::new(
        DefaultLoader,
        Size {
            width: 1024.0,
            height: 768.0,
        },
    );
    let url = "https://the-internet.herokuapp.com/login".parse().unwrap();
    session.load(url).expect("login page loads");
    let document = &session.page().expect("page").document;
    let username = document
        .get_element_by_id("username")
        .expect("username field");
    let password = document
        .get_element_by_id("password")
        .expect("password field");
    session.set_form_value(username, "tomsmith");
    session.set_form_value(password, "SuperSecretPassword!");
    session.submit_form(username).expect("submit");

    let final_url = session.current_url().expect("url").to_string();
    let document = &session.page().expect("page").document;
    let body_text = document.text_content(document.root());
    println!("final url: {final_url}");
    let success =
        final_url.contains("/secure") && body_text.contains("You logged into a secure area");
    println!("login: {}", if success { "SUCCESS" } else { "FAILED" });
    if !success {
        let head: String = body_text
            .split_whitespace()
            .take(40)
            .collect::<Vec<_>>()
            .join(" ");
        println!("page text: {head}");
    }
}
