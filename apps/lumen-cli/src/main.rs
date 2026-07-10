use lumen_engine::{Size, build_page, dump_layout};
use lumen_platform::{DefaultLoader, ResourceLoader, ResourceRequest, url_from_user_input};
use std::env;
use std::fs;
use std::path::Path;

const VIEWPORT: Size = Size {
    width: 1024.0,
    height: 768.0,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

/// Reads an input that may be a filesystem path or an http(s)/file URL.
fn read_input(input: &str) -> Result<String, Box<dyn std::error::Error>> {
    let url = url_from_user_input(input)?;
    Ok(DefaultLoader.load(&ResourceRequest { url })?.text())
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    match arguments
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["parse-html", input] => {
            let source = read_input(input)?;
            for token in lumen_html::tokenize(&source) {
                println!("{token:?}");
            }
        }
        ["parse-css", input] => {
            let source = read_input(input)?;
            println!("{:#?}", lumen_css::parse_stylesheet(&source));
        }
        ["dump-dom", input] => {
            print!("{}", lumen_html::parse_document(&read_input(input)?).dump());
        }
        ["dump-style", input] => {
            let page = build_page(&read_input(input)?, VIEWPORT);
            print!(
                "{}",
                lumen_engine::style::dump_styles(&page.document, &page.styles)
            );
        }
        ["dump-layout", input] => {
            let page = build_page(&read_input(input)?, VIEWPORT);
            print!("{}", dump_layout(&page.layout));
        }
        ["dump-display-list", input] => {
            let page = build_page(&read_input(input)?, VIEWPORT);
            print!(
                "{}",
                lumen_engine::paint::dump_display_list(&page.display_list)
            );
        }
        ["render", input, output] => {
            let page = build_page(&read_input(input)?, VIEWPORT);
            let svg = lumen_engine::render_svg(&page);
            if let Some(parent) = Path::new(output).parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(output, svg)?;
            println!("rendered {input} -> {output}");
        }
        _ => {
            print_usage();
            std::process::exit(2);
        }
    }
    Ok(())
}

fn print_usage() {
    eprintln!(
        "Lumen CLI — inspect every stage of the rendering pipeline\n\n\
         Commands:\n\
           parse-html <file>         HTML token stream\n\
           parse-css <file>          parsed stylesheet rules\n\
           dump-dom <file>           DOM tree\n\
           dump-style <file>         computed styles per element\n\
           dump-layout <file>        layout tree with box geometry\n\
           dump-display-list <file>  paint commands in order\n\
           render <file> <out.svg>   render to SVG\n\n\
         <file> may be a local path or an http(s):// URL."
    );
}
