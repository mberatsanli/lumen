use lumen_engine::{Size, build_page, dump_layout, render_svg};
use std::env;
use std::fs;
use std::path::Path;

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    match arguments.as_slice() {
        [command, input] if command == "parse-html" => {
            let source = fs::read_to_string(input)?;
            let document = lumen_html::parse_document(&source)?;
            print!("{}", document.dump());
        }
        [command, input] if command == "parse-css" => {
            let source = fs::read_to_string(input)?;
            let stylesheet = lumen_css::parse_stylesheet(&source)?;
            println!("{stylesheet:#?}");
        }
        [command, input] if command == "dump-layout" => {
            let source = fs::read_to_string(input)?;
            let page = build_page(
                &source,
                Size {
                    width: 1024.0,
                    height: 768.0,
                },
            )?;
            print!("{}", dump_layout(&page.layout));
        }
        [command, input, output] if command == "render" => {
            let source = fs::read_to_string(input)?;
            let page = build_page(
                &source,
                Size {
                    width: 1024.0,
                    height: 768.0,
                },
            )?;
            let svg = render_svg(&page);
            if let Some(parent) = Path::new(output).parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(output, svg)?;
            println!("rendered {input} -> {output}");
        }
        _ => print_usage(),
    }
    Ok(())
}

fn print_usage() {
    eprintln!(
        "Lumen CLI\n\n\
         Commands:\n\
           parse-html <file>\n\
           parse-css <file>\n\
           dump-layout <html-file>\n\
           render <html-file> <output.svg>"
    );
}
