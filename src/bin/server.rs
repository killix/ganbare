#![feature(inclusive_range_syntax)]

extern crate ganbare;
extern crate pencil;
extern crate dotenv;
#[macro_use] extern crate log;
extern crate env_logger;
extern crate hyper;
#[macro_use]  extern crate lazy_static;
#[macro_use]  extern crate mime;
extern crate time;
extern crate rustc_serialize;
extern crate rand;

use rand::thread_rng;
use dotenv::dotenv;
use std::env;
use ganbare::errors::*;
use std::net::IpAddr;

use std::collections::BTreeMap;
use hyper::header::{SetCookie, CookiePair, Cookie};
use pencil::{Pencil, Request, Response, PencilResult, redirect, abort, jsonify};
use pencil::helpers::send_file;
use ganbare::models::{User, Session};

lazy_static! {
    static ref SITE_DOMAIN : String = { dotenv().ok(); env::var("GANBARE_SITE_DOMAIN")
    .unwrap_or_else(|_| "".into()) };
}


pub fn get_cookie(cookies : &Cookie) -> Option<&str> {
    for c in cookies.0.iter() {
        if c.name == "session_id" {
            return Some(c.value.as_ref());
        }
    };
    None
}

fn get_user(conn : &ganbare::PgConnection, req : &Request) -> Result<Option<(User, Session)>> {
    if let Some(session_id) = req.cookies().and_then(get_cookie) {
        ganbare::check_session(&conn, session_id)
            .map(|user_sess| Some(user_sess))
            .or_else(|e| match e.kind() {
                &ErrorKind::BadSessId => Ok(None),
                &ErrorKind::NoSuchSess => Ok(None),
                _ => Err(e),
            })
    } else {
        Ok(None)
    }
}

trait ResponseExt {
    fn refresh_cookie(mut self, &ganbare::PgConnection, &Session, IpAddr) -> Self;
    fn expire_cookie(mut self) -> Self;
}

impl ResponseExt for Response {

fn refresh_cookie(mut self, conn: &ganbare::PgConnection, old_sess : &Session, ip: IpAddr) -> Self {
    let sess = ganbare::refresh_session(&conn, old_sess.sess_id.as_slice(), ip).expect("Session is already checked to be valid");

    let mut cookie = CookiePair::new("session_id".to_owned(), ganbare::sess_to_hex(&sess));
    cookie.path = Some("/".to_owned());
    cookie.domain = Some(SITE_DOMAIN.to_owned());
    cookie.expires = Some(time::now_utc() + time::Duration::weeks(2));
    self.set_cookie(SetCookie(vec![cookie]));
    self
}

fn expire_cookie(mut self) -> Self {
    let mut cookie = CookiePair::new("session_id".to_owned(), "".to_owned());
    cookie.path = Some("/".to_owned());
    cookie.domain = Some(SITE_DOMAIN.to_owned());
    cookie.expires = Some(time::at_utc(time::Timespec::new(0, 0)));
    self.set_cookie(SetCookie(vec![cookie]));
    self
}
}

fn hello(request: &mut Request) -> PencilResult {
    let conn = ganbare::db_connect()
        .map_err(|_| abort(500).unwrap_err())?;
    let user_session = get_user(&conn, &*request).map_err(|_| abort(500).unwrap_err())?;

    let mut context = BTreeMap::new();
    context.insert("title".to_string(), "akusento.ganba.re".to_string());

    match user_session {
        Some((_, sess)) => request.app.render_template("main.html", &context)
                            .map(|resp| resp.refresh_cookie(&conn, &sess, request.remote_addr().ip())),
        None => request.app.render_template("hello.html", &context),
    }
}

fn login(request: &mut Request) -> PencilResult {
    let app = request.app;
    let ip = request.request.remote_addr.ip();
    let login_form = request.form_mut();
    let email = login_form.take("email").unwrap_or_default();
    let plaintext_pw = login_form.take("password").unwrap_or_default();

    let mut context = BTreeMap::new();
    context.insert("title".to_string(), "akusento.ganba.re".to_string());
    context.insert("authError".to_string(), "true".to_string());

    do_login(&email, &plaintext_pw, ip)
        .or_else(|e| match e {
            pencil::PencilError::PenHTTPError(pencil::HTTPError::Unauthorized) => {
                let result = app.render_template("hello.html", &context);
                result.map(|mut resp| {resp.status_code = 401; resp})
            },
            _ => Err(e),
        })
}

fn do_login(email : &str, plaintext_pw : &str, ip : IpAddr) -> PencilResult {
    let conn = ganbare::db_connect().map_err(|_| abort(500).unwrap_err())?;
    let user;
    {
        user = ganbare::auth_user(&conn, email, plaintext_pw)
            .map_err(|e| match e.kind() {
                    &ErrorKind::AuthError => abort(401).unwrap_err(),
                    _ => abort(500).unwrap_err(),
                })?;
    };

    let session = ganbare::start_session(&conn, &user, ip)
        .map_err(|_| abort(500).unwrap_err())?;

    redirect("/", 303).map(|resp| resp.refresh_cookie(&conn, &session, ip) )
}


fn logout(request: &mut Request) -> PencilResult {
    let conn = ganbare::db_connect().map_err(|_| abort(500).unwrap_err())?;
    if let Some(session_id) = request.cookies().and_then(get_cookie) {
        ganbare::end_session(&conn, &session_id)
            .map_err(|_| abort(500).unwrap_err())?;
    };

    redirect("/", 303).map(ResponseExt::expire_cookie)
}

fn error(err_msg : &str) -> pencil::PencilError {
    println!("Error: {}", err_msg);
    abort(500).unwrap_err()
}


fn confirm(request: &mut Request) -> PencilResult {

    let secret = request.args().get("secret")
        .ok_or_else(|| error("Can't get argument secret from URL!") )?;
    let conn = ganbare::db_connect()
        .map_err(|_| error("Can't connect to database!") )?;
    let email = ganbare::check_pending_email_confirm(&conn, &secret)
        .map_err(|_| error("Check pending email confirms failed!"))?;

    let mut context = BTreeMap::new();
    context.insert("title".to_string(), "akusento.ganba.re".to_string());
    context.insert("email".to_string(), email);
    context.insert("secret".to_string(), secret.clone());

    request.app.render_template("confirm.html", &context)
}

fn confirm_final(request: &mut Request) -> PencilResult {
    let ip = request.request.remote_addr.ip();
    let conn = ganbare::db_connect()
        .map_err(|_| abort(500).unwrap_err())?;
    let secret = request.args().get("secret")
            .ok_or_else(|| abort(500).unwrap_err() )?.clone();
    let password = request.form_mut().get("password")
        .ok_or_else(|| abort(500).unwrap_err() )?;
    let user = ganbare::complete_pending_email_confirm(&conn, password, &secret).map_err(|_| abort(500).unwrap_err())?;

    do_login(&user.email, &password, ip)
}

#[derive(RustcEncodable)]
struct Quiz {
    username: String,
    lines: String,
}

fn new_quiz(req: &mut Request) -> PencilResult {
    let conn = ganbare::db_connect()
        .map_err(|_| abort(500).unwrap_err())?;
    let (user, sess) = get_user(&conn, req)
        .map_err(|_| abort(500).unwrap_err())?
        .ok_or_else(|| abort(401).unwrap_err())?; // Unauthorized

    let quiz_path = ganbare::get_new_quiz(&conn, &user);
    let line_path = "/api/get_line/".to_string() + &quiz_path
        .map_err(|_| abort(500).unwrap_err())?;
 
    jsonify(&Quiz { username: user.email,lines: line_path })
        .map(|resp| resp.refresh_cookie(&conn, &sess, req.remote_addr().ip()))
}

fn get_line(req: &mut Request) -> PencilResult {
    let conn = ganbare::db_connect()
        .map_err(|_| abort(500).unwrap_err())?;
    let (_, sess) = get_user(&conn, req)
        .map_err(|_| abort(500).unwrap_err())?
        .ok_or_else(|| abort(401).unwrap_err() )?; // Unauthorized

    let line_id = req.view_args.get("line_id").expect("Pencil guarantees that Line ID should exist as an arg.");
    let line_id = line_id.parse::<i32>().expect("Pencil guarantees that Line ID should be an integer.");
    let (file_path, mime_type) = ganbare::get_line_file(&conn, line_id)
        .map_err(|e| {
            match e.kind() {
                &ErrorKind::FileNotFound => abort(404).unwrap_err(),
                _ => abort(500).unwrap_err(),
            }
        })?;

    send_file(&file_path, mime_type, false)
        .map(|resp| resp.refresh_cookie(&conn, &sess, req.remote_addr().ip()))
}

fn add_quiz_form(req: &mut Request) -> PencilResult {
    let conn = ganbare::db_connect()
        .map_err(|_| abort(500).unwrap_err())?;
    let user_session = get_user(&conn, &*req).map_err(|_| abort(500).unwrap_err())?;

    match user_session {
        Some((_, sess)) => {

            let mut context = BTreeMap::new();
            context.insert("title".to_string(), "akusento.ganba.re".to_string());
            req.app.render_template("add_quiz.html", &context)
                            .map(|resp| resp.refresh_cookie(&conn, &sess, req.remote_addr().ip()))
            },
        None => abort(401),
    }
}

fn add_quiz_post(req: &mut Request) -> PencilResult  {

    fn parse_form(req: &mut Request) -> Result<(String, String, String, Vec<ganbare::Fieldset>)> {

        macro_rules! parse {
            ($expression:expr) => {$expression.map(String::to_string).ok_or(ErrorKind::FormParseError.to_err())?;}
        }
        req.load_form_data();
        let form = req.form().expect("Form data should be loaded!");
        let files = req.files().expect("Form data should be loaded!");;

        let lowest_fieldset = str::parse::<i32>(&parse!(form.get("lowest_fieldset")))?;
        if lowest_fieldset > 10 { return Err(ErrorKind::FormParseError.to_err()); }

        let question_name = parse!(form.get("name"));
        let question_explanation = parse!(form.get("explanation"));
        let skill_nugget = parse!(form.get("skill_nugget"));

        let mut fieldsets = Vec::with_capacity(lowest_fieldset as usize);
        for i in 1...lowest_fieldset {

            let q_variations = str::parse::<i32>(&parse!(form.get(&format!("choice_{}_q_variations", i))))?;
            if lowest_fieldset > 100 { return Err(ErrorKind::FormParseError.to_err()); }

            let mut q_variants = Vec::with_capacity(q_variations as usize);
            for v in 1...q_variations {
                if let Some(file) = files.get(&format!("choice_{}_q_variant_{}", i, v)) {
                    if file.size.expect("Size should've been parsed at this phase.") == 0 {
                        continue; // Don't save files with size 0;
                    }
                    let mut file = file.clone();
                    file.do_not_delete_on_drop();
                    q_variants.push(
                        (file.path.clone(),
                        file.filename().map_err(|_| ErrorKind::FormParseError.to_err())?,
                        file.content_type().ok_or(ErrorKind::FormParseError.to_err())?)
                    );
                }
            }
            let answer_audio = files.get(&format!("choice_{}_answer_audio", i));
            let answer_audio_path;
            if let Some(path) = answer_audio {
                if path.size.expect("Size should've been parsed at this phase.") == 0 {
                    answer_audio_path = None;
                } else {
                    let mut cloned_path = path.clone();
                    cloned_path.do_not_delete_on_drop();
                    answer_audio_path = Some(
                        (cloned_path.path.clone(),
                        cloned_path.filename().map_err(|_| ErrorKind::FormParseError.to_err())?,
                        cloned_path.content_type().ok_or(ErrorKind::FormParseError.to_err())?)
                    )
                }
            } else {
                answer_audio_path = None;
            };

            let answer_text = parse!(form.get(&format!("choice_{}_answer_text", i)));
            let fields = ganbare::Fieldset {q_variants: q_variants, answer_audio: answer_audio_path, answer_text: answer_text};
            fieldsets.push(fields);
        }

        Ok((question_name, question_explanation, skill_nugget, fieldsets))
    }

    fn move_to_new_path(path: &mut std::path::PathBuf, orig_filename: Option<&str>) -> Result<()> {
        use rand::Rng;
        let mut new_path = std::path::PathBuf::from("audio/");
        let mut filename = "%FT%H-%M-%SZ".to_string();
        filename.extend(thread_rng().gen_ascii_chars().take(10));
        println!("Extension: {:?}, {:?}", path, path.extension());
        filename.push_str(".");
        filename.push_str(std::path::Path::new(orig_filename.unwrap_or("")).extension().and_then(|s| s.to_str()).unwrap_or("noextension"));
        new_path.push(time::strftime(&filename, &time::now()).unwrap());
        std::fs::rename(&*path, &new_path)?;
        std::mem::swap(path, &mut new_path);
        Ok(())
    }

    let conn = ganbare::db_connect()
        .map_err(|_| abort(500).unwrap_err())?;
    let user_session = get_user(&conn, &*req).map_err(|_| abort(500).unwrap_err())?;


    match user_session {
        Some((_, sess)) => {

            let mut form = parse_form(&mut *req).map_err(|ee| { println!("Error: {:?}", ee); abort(500).unwrap_err()})?;
            for f in &mut form.3 {
                if let Some((ref mut temp_path, ref mut filename, _)) = f.answer_audio {
                    move_to_new_path(temp_path, filename.as_ref().map(|s| s.as_str()))
                        .map_err(|_| abort(500).unwrap_err())?;
                }
                for &mut (ref mut temp_path, ref mut filename, _) in &mut f.q_variants {
                    move_to_new_path(temp_path, filename.as_ref().map(|s| s.as_str()))
                        .map_err(|_| abort(500).unwrap_err())?;
                }
            }

            let result = ganbare::create_quiz(&conn, form);
            result.map_err(|_| abort(500).unwrap_err())?;

            redirect("/add_quiz", 303).map(|resp| resp.refresh_cookie(&conn, &sess, req.remote_addr().ip()) )

            },
        None => abort(401),
    }
}


fn main() {
    dotenv().ok();
    let mut app = Pencil::new(".");
    app.register_template("hello.html");
    app.register_template("main.html");
    app.register_template("confirm.html");
    app.register_template("add_quiz.html");
    app.enable_static_file_handling();

 //   app.set_debug(true);
 //   app.set_log_level();
 //   env_logger::init().unwrap();
    debug!("* Running on http://localhost:5000/, serving at {:?}", *SITE_DOMAIN);

    app.get("/", "hello", hello);
    app.post("/logout", "logout", logout);
    app.post("/login", "login", login);
    app.get("/confirm", "confirm", confirm);
    app.get("/add_quiz", "add_quiz_form", add_quiz_form);
    app.post("/add_quiz", "add_quiz_post", add_quiz_post);
    app.post("/confirm", "confirm_final", confirm_final);
    app.get("/api/new_quiz", "new_quiz", new_quiz);
    app.get("/api/get_line/<line_id:int>", "get_line", get_line);

    let binding = match env::var("GANBARE_SERVER_BINDING") {
        Err(_) => {
            println!("Specify the ip address and port to listen (e.g. 0.0.0.0:80) in envvar GANBARE_SERVER_BINDING!");
            return;
        },
        Ok(ok) => ok,
    };
    println!("Ready to run at {}", binding);
    app.run(binding.as_str());
}
