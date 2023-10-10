use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Ok, Result};
use regex::Regex;
use reqwest::{Client, Response, Url};
use scraper::{Html, Selector};
use structopt::StructOpt;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::task::JoinHandle;
use urlencoding::decode;

fn extract_links_from_html(html: &str) -> Vec<String> {
    let fragment = Html::parse_fragment(html);
    let link_selector = Selector::parse("link[href]").unwrap();
    let anchor_selector = Selector::parse("a[href]").unwrap();
    let image_selector = Selector::parse("img[src]").unwrap();
    let script_selector = Selector::parse("script[src]").unwrap();

    let mut links = fragment
        .select(&link_selector)
        .map(|element| element.value().attr("href").unwrap().to_string())
        .collect::<Vec<String>>();
    let mut anchors = fragment
        .select(&anchor_selector)
        .map(|element| element.value().attr("href").unwrap().to_string())
        .collect::<Vec<String>>();
    let mut images = fragment
        .select(&image_selector)
        .map(|element| element.value().attr("src").unwrap().to_string())
        .collect::<Vec<String>>();
    let mut scripts = fragment
        .select(&script_selector)
        .map(|element| element.value().attr("src").unwrap().to_string())
        .collect::<Vec<String>>();

    links.append(&mut anchors);
    links.append(&mut images);
    links.append(&mut scripts);
    links.sort_unstable_by(|a, b| a.cmp(b));
    links.dedup();
    links
}

fn extract_links_from_css(css: &str) -> Vec<String> {
    let pattern = Regex::new(r"url\('[\./\w\-\\%=]+'\)").unwrap();
    let urls: Vec<String> = pattern
        .find_iter(css)
        .map(|m| {
            let link = m.as_str().to_string();
            decode(&link.replace("url(\'", "").replace("\')", ""))
                .expect("UTF-8")
                .into_owned()
        })
        .collect();
    urls
}

fn concat_two_paths(path1: String, path2: String) -> String {
    format!("{}/{}", path1, path2).trim_matches('/').to_string()
}

async fn download_file(
    opt: Opt,
    url: Url,
    queue: Arc<Mutex<VecDeque<String>>>,
    downloaded_files: Arc<Mutex<Vec<String>>>,
    visited_urls: Arc<Mutex<Vec<String>>>,
    client: Arc<Mutex<Client>>,
) -> Result<()> {
    let url_string = url.to_string().replace("?", "%3F");
    let get = client.lock().unwrap().get(url_string);
    let response = get.send().await?;
    if is_text_file(url.to_string(), &response).await? {
        let relative_url = get_relative_url_from_url(url.clone());
        let url_string = url.to_string();
        let text_file = response.text().await?;
        if url_string.ends_with(".html") || url_string.ends_with(".htm") {
            let links = extract_links_from_html(&text_file);
            add_link_to_queue(links, relative_url, queue, &downloaded_files, visited_urls)?;
        } else if url_string.ends_with(".css") {
            let links = extract_links_from_css(&text_file);
            add_link_to_queue(links, relative_url, queue, &downloaded_files, visited_urls)?;
        }
        save_text_to_file(&opt, &url, text_file).await?;
    } else {
        save_binary_to_file(&opt, &url, response).await?;
    }
    downloaded_files.lock().unwrap().push(url.to_string());
    println!("downloaded file: {}", url.clone());

    Ok(())
}

fn add_link_to_queue(
    links: Vec<String>,
    relative_url: Url,
    queue: Arc<Mutex<VecDeque<String>>>,
    downloaded_files: &Arc<Mutex<Vec<String>>>,
    visited_urls: Arc<Mutex<Vec<String>>>,
) -> Result<(), anyhow::Error> {
    Ok(for link in links {
        let new_url = relative_url.join(link.as_str())?;
        let file_path = get_file_path_from_url(&new_url);
        if queue.lock().unwrap().contains(&file_path.clone())
            || downloaded_files
                .lock()
                .unwrap()
                .contains(&file_path.clone())
            || visited_urls.lock().unwrap().contains(&new_url.to_string())
        {
            continue;
        }
        queue.lock().unwrap().push_back(file_path.clone());
    })
}

fn get_relative_url_from_url(url: Url) -> Url {
    let mut url = Url::parse(
        format!(
            "{}://{}{}",
            url.scheme(),
            url.host_str().unwrap(),
            url.path()
        )
        .as_str(),
    )
    .unwrap();

    clean_url_path(&mut url);
    url
}

fn clean_url_path(url: &mut Url) -> &mut Url {
    let pattern = Regex::new(r"/+").unwrap();
    let new_path = pattern.replace_all(url.path(), "/").to_string();
    url.set_path(new_path.as_str());
    url
}

fn get_folder_path_from_url(url: &Url) -> String {
    let file_path = get_file_path_from_url(url);
    if file_path.contains("/") {
        let parts: Vec<String> = file_path.split("/").map(|part| part.to_string()).collect();
        return parts
            .iter()
            .take(parts.len() - 1)
            .map(|part| part.clone())
            .collect::<Vec<String>>()
            .join("/");
    }
    return String::new();
}

fn get_file_path_from_url(url: &Url) -> String {
    let url_str = url.to_string();
    decode(&url_str.replace(
        &format!("{}://{}/", url.scheme(), url.host_str().unwrap()),
        "",
    ))
    .expect("UTF-8")
    .into_owned()
}

async fn save_binary_to_file(opt: &Opt, url: &Url, response: Response) -> Result<()> {
    let bytes = response.bytes().await?;
    let folder_path = get_folder_path_from_url(url);
    let complete_path = concat_two_paths(opt.output.clone(), folder_path);
    std::fs::create_dir_all(format!("{}", complete_path))?;
    let file_path = get_file_path_from_url(url);
    let mut file = File::create(format!("{}/{}", opt.output.clone(), file_path)).await?;
    file.write_all(&bytes).await?;

    Ok(())
}

async fn save_text_to_file(opt: &Opt, url: &Url, file_str: String) -> Result<()> {
    let folder_path = get_folder_path_from_url(url);
    let complete_path = concat_two_paths(opt.output.clone(), folder_path);
    let adjusted_complete_path = complete_path.replace("http://", "");
    std::fs::create_dir_all(format!("{}", adjusted_complete_path))?;
    let file_path = get_file_path_from_url(url);
    let mut file = File::create(format!("{}/{}", opt.output.clone(), file_path)).await?;
    for line in file_str.clone().split('\n') {
        file.write_all(line.as_bytes()).await?;
        file.write_all("\n".as_bytes()).await?;
    }
    file.flush().await?;

    Ok(())
}

async fn is_text_file(url: String, response: &Response) -> Result<bool> {
    let font_files = vec![".woff", ".eot", ".ttf"];
    for font_file in font_files {
        if url.contains(font_file) {
            return Ok(false);
        }
    }

    let content_type = response.headers().get(reqwest::header::CONTENT_TYPE);
    if let Some(content_type) = content_type {
        let content_type_str = content_type.to_str()?;
        if content_type_str.starts_with("text/") {
            return Ok(true);
        }
    }

    Ok(false)
}

const WEBPAGE_DUMPER: &str = r#"webpage dumper"#;
const URI_HELP: &str = r#"Uri to the page that you want to dump files from"#;
const OUTPUT_DEFAULT_VALUE: &str = r#"output"#;
const OUTPUT_HELP: &str = r#"Output folder where to download into"#;
const PARSE_INDEX_HTLM_SPINNER_TEXT: &str = r#"Parsing index.html for file resources"#;
const NUMBER_OF_FILES_FOUND_SPINNER_TEXT: &str = r#"No files where found while parsing index.html"#;
const MUSIC_HELP: &str = r#"If you want music to be played or not"#;

#[derive(StructOpt, Debug, Clone)]
#[structopt(name = WEBPAGE_DUMPER)]
struct Opt {
    #[structopt(short, long, help = URI_HELP)]
    uri: String,

    #[structopt(
        short,
        long,
        default_value = OUTPUT_DEFAULT_VALUE,
        help = OUTPUT_HELP
    )]
    output: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let opt: Opt = Opt::from_args();

    let url = Url::parse(opt.uri.clone().as_str())?;
    let response = reqwest::get(url.clone()).await?;
    let html = response.text().await?;
    let links: Vec<String> = extract_links_from_html(&html)
        .iter()
        .filter(|link| **link != "index.html".to_string())
        .map(|link| link.clone())
        .collect();

    let queue = Arc::new(Mutex::new(VecDeque::<String>::new()));
    let downloaded_files = Arc::new(Mutex::new(Vec::<String>::new()));
    let visited_urls = Arc::new(Mutex::new(Vec::<String>::new()));
    let client = Arc::new(Mutex::new(reqwest::Client::new()));

    for link in links {
        queue.lock().unwrap().push_back(link);
    }

    let mut tasks: Vec<JoinHandle<Result<()>>> = vec![];

    loop {
        let link_maybe = queue.lock().unwrap().pop_front();
        tasks.retain(|task| !task.is_finished());
        if let Some(link) = link_maybe {
            let url = Url::parse(format!("{}{}", opt.uri, link).as_str())?;
            if visited_urls.lock().unwrap().contains(&url.to_string()) {
                continue;
            } else {
                visited_urls.lock().unwrap().push(url.to_string());
            }
            tasks.push(tokio::spawn(download_file(
                opt.clone(),
                url,
                queue.clone(),
                downloaded_files.clone(),
                visited_urls.clone(),
                client.clone(),
            )));
        } else {
            let is_not_done = tasks.iter().any(|task| !task.is_finished());
            if is_not_done {
                thread::sleep(Duration::from_secs(4));
                continue;
            } else {
                break;
            }
        }
    }

    let index_url = url.join("index.html")?;
    save_text_to_file(&opt, &index_url, html).await?;

    let results = futures::future::join_all(tasks).await;

    for result in results {
        if let Err(e) = result {
            eprintln!("Error downloading file: {:?}", e);
        }
    }

    Ok(())
}
