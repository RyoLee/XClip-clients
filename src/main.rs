#![windows_subsystem = "windows"]
extern crate base64;

use base64::{decode, encode};
use clipboard::ClipboardContext;
use clipboard::ClipboardProvider;
use ini::Ini;
use md5::compute;
use std::env;
use std::io;
use std::path::PathBuf;

fn get_cfg_path() -> io::Result<PathBuf> {
    let mut dir = env::current_exe()?;
    dir.pop();
    dir.push("xclip.ini");
    Ok(dir)
}

fn main() -> Result<(), ureq::Error> {
    let conf = Ini::load_from_file(get_cfg_path().expect("miss config!").to_path_buf()).unwrap();
    let base_url = conf.get_from(Some("main"), "server").unwrap();
    let id = conf.get_from(Some("main"), "id").unwrap();
    let password = conf.get_from(Some("main"), "password").unwrap();
    let mut url = base_url.to_string() + &"/".to_string();
    let id_hash = format!("{:x}", compute(id.as_bytes())).to_string();
    url += &id_hash;
    let mut args = env::args();
    if args.len() == 2 {
        let mode = args.nth(1).unwrap();
        if mode == "c" {
            println!("{}", set(&url, password).unwrap());
        }
        if mode == "v" {
            println!("{}", get(&url, password).unwrap());
        }
    }
    return Ok(());
}
fn get(url: &str, password: &str) -> Result<String, ureq::Error> {
    let password_hash = format!("{:x}", compute(password.as_bytes()));
    let body: String = ureq::get(url)
        .set("password", &password_hash)
        .call()?
        .into_string()?;
    let db = decode(&body.to_string()).unwrap();
    let res = String::from_utf8_lossy(&db);
    println!("{}", res.to_string());
    let mut ctx: ClipboardContext = ClipboardProvider::new().unwrap();
    ctx.set_contents(res.to_string()).unwrap();
    return Ok(res.to_string());
}
fn set(url: &str, password: &str) -> Result<String, ureq::Error> {
    let password_hash = format!("{:x}", compute(password.as_bytes()));
    let mut ctx: ClipboardContext = ClipboardProvider::new().unwrap();
    let raw_value = ctx.get_contents().unwrap();
    let value = encode(raw_value);
    let body: String = ureq::post(url)
        .set("password", &password_hash)
        .send_form(&[("value", &value)])?
        .into_string()?;
    return Ok(body);
}
