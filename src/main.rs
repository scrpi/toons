use chrono::prelude::*;
use clap::{Parser, Subcommand};
use clap_verbosity_flag::Verbosity;
use dotenv::dotenv;
use futures::future::join_all;
use log::{debug, error, info};
use rfesi::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::net::TcpListener;

const TOONS_FILE: &str = "toons.json";

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(flatten)]
    verbose: Verbosity,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    List,
    Show { name: String },
    Auth,
    Refresh { name: String },
    Stats { name: Option<String> },
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
struct CharacterData {
    name: String,
    id: i32,
    refresh_token: String,
    scopes: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct EsiCallbackParams {
    code: String,
    state: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct EsiVerifyResponse {
    #[serde(rename = "CharacterID")]
    character_id: i32,
    character_name: String,
    scopes: String,
}

fn create_esi() -> EsiResult<Esi> {
    let client_id = std::env::var("ESI_CLIENT_ID").expect("ESI_CLIENT_ID must be set.");
    let secret = std::env::var("ESI_SECRET").expect("ESI_CLIENT_SECRET must be set.");
    EsiBuilder::new()
        .user_agent("eve-toons-agent")
        .client_id(&client_id)
        .client_secret(&secret)
        .callback_url("http://localhost:5000/esi/callback")
        .scope("esi-characterstats.read.v1 esi-skills.read_skills.v1 esi-skills.read_skillqueue.v1")
        .build()
}

fn write_toons(toons: &HashMap<String, CharacterData>) {
    let toon_file = File::create(TOONS_FILE).expect("Create file");
    let mut writer = BufWriter::new(toon_file);
    serde_json::to_writer_pretty(&mut writer, &toons).expect("Serialize");
    writer.flush().expect("Flush writer");
}

fn read_toons() -> HashMap<String, CharacterData> {
    let file = match File::open(TOONS_FILE) {
        Ok(file) => file,
        Err(_) => {
            let ret: HashMap<String, CharacterData> = HashMap::new();
            return ret;
        }
    };
    let reader = BufReader::new(file);
    serde_json::from_reader(reader).unwrap()
}

fn auth_cb() -> Option<EsiCallbackParams> {
    let mut params = None;
    let listener = TcpListener::bind("127.0.0.1:5000").unwrap();
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                let mut line = String::new();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    match reader.read_line(&mut line) {
                        Ok(_) => {
                            if line.starts_with("GET /esi/callback?") {
                                let (query, _) = line
                                    .strip_prefix("GET /esi/callback?")
                                    .unwrap()
                                    .split_once(' ')
                                    .unwrap();
                                params = Some(serde_qs::from_str(query).unwrap());
                            }
                            if line == "\r\n" {
                                break;
                            }
                            line.clear()
                        }
                        Err(e) => {
                            error!("Encountered IO error: {}", e);
                        }
                    }
                }
                let response = b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=UTF-8\r\n\r\n<html><body>OK</body></html>\r\n";
                match stream.write(response) {
                    Ok(_) => (),
                    Err(e) => error!("Failed sending response: {}", e),
                }
                break;
            }
            Err(e) => {
                error!("Unable to accept: {e}");
            }
        }
    }
    params
}

async fn do_auth() {
    let mut toons = read_toons();

    loop {
        let mut esi = create_esi().unwrap();
        let url = esi.get_authorize_url().unwrap().authorization_url;
        println!("Authenticating. {}", url);

        let params = auth_cb().expect("Auth callback failed");
        let _ = esi.authenticate(&params.code, None).await;
        let client = reqwest::Client::new();
        let request_url = "https://login.eveonline.com/oauth/verify";
        let response = client
            .get(request_url)
            .bearer_auth(esi.access_token.clone().unwrap())
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let verify: EsiVerifyResponse = serde_json::from_str(&response).unwrap();
        debug!("{:#?}", verify);
        toons.insert(
            verify.character_name.clone(),
            CharacterData {
                name: verify.character_name,
                id: verify.character_id,
                refresh_token: esi.refresh_token.clone().unwrap(),
                scopes: verify.scopes,
            },
        );
        write_toons(&toons);
    }
}

fn find_toon<'a>(
    toons: &'a HashMap<String, CharacterData>,
    name: &str,
) -> Option<&'a CharacterData> {
    match toons.get(name) {
        Some(toon) => {
            return Some(toon);
        }
        None => {
            for (toon_name, toon_data) in toons {
                if toon_name.starts_with(name) {
                    return Some(toon_data);
                }
            }
        }
    }
    None
}

fn do_show(name: &str) {
    let toons = read_toons();
    match find_toon(&toons, name) {
        Some(toon) => {
            println!("{toon:#?}");
        }
        None => {
            println!("No Character '{name}' found");
        }
    }
}

async fn do_refresh(name: &str) {
    let toons = read_toons();
    match find_toon(&toons, name) {
        Some(toon) => {
            println!("{toon:#?}");
            let mut esi = create_esi().unwrap();
            println!("{:#?}", esi);
            let result = esi.refresh_access_token(Some(&toon.refresh_token)).await;
            println!("{:#?}", result);
            println!("{:#?}", esi);
        }
        None => {
            println!("No Character '{name}' found");
        }
    }
}

static CROP_SKILLS: [i32; 7] = [3412, 3551, 13278, 21718, 25739, 25810, 25811];

#[derive(Debug)]
struct CropStat {
    name: String,
    points: i64,
    training: bool,
    queued: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct QueuedSkill {
    queue_position: i32,
    skill_id: i32,
    finished_level: i32,
    start_date: String,
    finish_date: String,
    training_start_sp: i32,
    level_start_sp: i32,
    level_end_sp: i32,
}

fn calculate_queued_skill(skill: &QueuedSkill) -> (bool, bool, i64) {
    if !CROP_SKILLS.contains(&skill.skill_id) {
        return (false, false, 0);
    }
    debug!("{:#?}", skill);
    let mut training = false;
    let mut is_crop = false;
    let mut points = 0;

    let now = Utc::now().timestamp() as f64;
    let skill_start = DateTime::parse_from_rfc3339(&skill.start_date)
        .unwrap()
        .timestamp() as f64;
    let skill_finish = DateTime::parse_from_rfc3339(&skill.finish_date)
        .unwrap()
        .timestamp() as f64;

    if skill_start < now {
        // Skill either completed training or currently in training
        if now > skill_finish {
            // Skill has finished training
            let sp = skill.level_end_sp - skill.training_start_sp;
            debug!("Trained SP: {:?}", sp);
            points += sp as i64;
        } else {
            // Skill currently in training
            training = true;
            debug!("{:?} {:?} {:?}", now, skill_start, skill_finish);
            let train_points = (skill.level_end_sp - skill.training_start_sp) as f64;
            let completed = now - skill_start;
            let total = skill_finish - skill_start;
            // pct is a value between 0.0 and 1.0 that represents progress towards
            // completing the current skill being trained
            let pct = completed / total;
            let sp = train_points * pct;
            debug!("{:?} * Training SP: {:?}", pct, sp);
            points += sp as i64;
        }
    } else {
        is_crop = true;
    }

    (training, is_crop, points)
}

async fn do_stats_single(data: CharacterData) -> Result<CropStat, String> {
    let mut stat = CropStat {
        name: data.name.clone(),
        points: 0,
        training: false,
        queued: 0,
    };
    let mut esi = create_esi().unwrap();

    info!("Refreshing API token for {}", data.name);
    let _ = esi.refresh_access_token(Some(&data.refresh_token)).await;
    let _ = esi.update_spec().await;

    info!("Pulling skills for {}", data.name);
    match esi.group_skills().get_skills(data.id).await {
        Ok(skills) => {
            for skill in skills.skills {
                if CROP_SKILLS.contains(&skill.skill_id) {
                    debug!("{:#?}", skill);
                    stat.points += skill.skillpoints_in_skill;
                }
            }
        }
        Err(e) => {
            return Err(format!("Error retrieving skills: {e}"));
        }
    }

    info!("Pulling queue for {}", data.name);
    let path = esi
        .get_endpoint_for_op_id("get_characters_character_id_skillqueue")
        .unwrap()
        .replace("{character_id}", &data.id.to_string());
    let queue = esi
        .query::<Vec<QueuedSkill>>("GET", RequestType::Authenticated, &path, None, None)
        .await;
    match queue {
        Ok(queue) => {
            for skill in queue {
                let (training, is_crop, points) = calculate_queued_skill(&skill);
                if training {
                    stat.training = true;
                }
                if is_crop {
                    stat.queued += 1;
                }
                stat.points += points;
            }
        }
        Err(e) => return Err(format!("Error retrieving skill queue: {e}")),
    }
    Ok(stat)
}

async fn do_stats(single_name: &Option<String>) {
    let mut crop_stats = Vec::new();
    let mut single = Vec::new();
    let toons = read_toons();

    if let Some(single_name) = single_name {
        // Single character stats
        if let Some(data) = find_toon(&toons, single_name) {
            single.push(do_stats_single(data.clone()));
        } else {
            error!("Could not find Character: {}", single_name);
        }
    } else {
        // All character stats
        for (_, data) in toons {
            single.push(do_stats_single(data.clone()));
        }
    }
    for result in join_all(single).await {
        match result {
            Ok(stat) => {
                crop_stats.push(stat);
            }
            Err(e) => {
                error!("{}", e);
            }
        }
    }
    crop_stats.sort_by_key(|s| s.points);
    crop_stats.reverse();
    println!("--- Results ---");
    let mut available_extracts = 0;
    for stat in crop_stats {
        available_extracts += stat.points / 500_000;
        let training = if stat.training { 1 } else { 0 };
        println!(
            "{}: {} points, {:.2} extractions, {} crop skill training, {} crop skills queued",
            stat.name,
            stat.points,
            stat.points as f64 / 500_000.0,
            training,
            stat.queued,
        );
    }
    println!("---");
    println!("Total available extractions: {}", available_extracts);
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    dotenv().ok();
    let cli = Cli::parse();

    env_logger::Builder::new()
        .filter_level(cli.verbose.log_level_filter())
        .init();

    match &cli.command {
        Some(Commands::List) => {
            let toons = read_toons();
            for (_, data) in toons {
                println!("{} :: {}", data.name, data.id);
            }
        }
        Some(Commands::Show { name }) => {
            do_show(name);
        }
        Some(Commands::Auth) => {
            do_auth().await;
        }
        Some(Commands::Refresh { name }) => {
            do_refresh(name).await;
        }
        Some(Commands::Stats { name }) => {
            do_stats(name).await;
        }
        None => {}
    }

    Ok(())
}
