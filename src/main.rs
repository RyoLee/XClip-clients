#![windows_subsystem = "windows"]

use base64::{decode, encode};
use curl::easy::Easy;
use curl::easy::Form;
use ini::Ini;
use md5::compute;
use clipboard::ClipboardProvider;
use clipboard::ClipboardContext;
use std::env;
use std::io;
use std::path::PathBuf;

fn get_cfg_path() -> io::Result<PathBuf> {
    let mut dir = env::current_exe()?;
    dir.pop();
    dir.push("xclip.ini");
    Ok(dir)
}
fn main() {
    let conf = Ini::load_from_file(get_cfg_path().expect("miss config!").to_path_buf()).unwrap();
    let base_url = conf.get_from(Some("main"), "server").unwrap();
    let key = conf.get_from(Some("main"), "key").unwrap();
    let password = conf.get_from(Some("main"), "password").unwrap();
    let mut args = env::args();
    if args.len() == 2 {
        let mode = args.nth(1).unwrap();
        if mode == "c" {
            set(key, password, base_url);
            return;
        }
        if mode == "v" {
            get(key, password, base_url);
            return;
        }
    }
}
fn get(key: &str, r_password: &str, base_url: &str) {
    let mut data = Vec::new();
    let mut handle = Easy::new();
    let url = base_url.to_string() + "/get";
    handle.post(true).unwrap();
    let mut form = Form::new();
    let password = format!("{:x}", compute(r_password.as_bytes()));
    form.part("key").contents(key.as_bytes()).add().unwrap();
    form.part("password").contents(password.as_bytes()).add().unwrap();
    handle.url(&url.to_string()[..]).unwrap();
    handle.post(true).unwrap();
    handle.httppost(form).unwrap();
    {
        let mut transfer = handle.transfer();
        transfer
            .write_function(|new_data| {
                data.extend_from_slice(new_data);
                Ok(new_data.len())
            })
            .unwrap();
        transfer.perform().unwrap();
    }
    let db = decode(data).unwrap();
    let res = String::from_utf8_lossy(&db);
    let mut ctx: ClipboardContext = ClipboardProvider::new().unwrap();
    ctx.set_contents(res.to_string()).unwrap();
}
fn set(key: &str, r_password: &str, base_url: &str) {
    let mut data = Vec::new();
    let mut handle = Easy::new();
    let url = base_url.to_string() + "/set";
    handle.post(true).unwrap();
    let mut form = Form::new();
    let mut ctx: ClipboardContext = ClipboardProvider::new().unwrap();
    let raw_value = ctx.get_contents().unwrap();
    let value = encode(raw_value);
    let password = format!("{:x}", compute(r_password.as_bytes()));
    form.part("key").contents(key.as_bytes()).add().unwrap();
    form.part("password").contents(password.as_bytes()).add().unwrap();
    form.part("value").contents(value.as_bytes()).add().unwrap();
    handle.url(&url.to_string()[..]).unwrap();
    handle.post(true).unwrap();
    handle.httppost(form).unwrap();
    {
        let mut transfer = handle.transfer();
        transfer
            .write_function(|new_data| {
                data.extend_from_slice(new_data);
                Ok(new_data.len())
            })
            .unwrap();
        transfer.perform().unwrap();
    }
}
