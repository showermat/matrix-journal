use clap::Parser;
use anyhow::Result;
use chrono;
use env_logger;
use log;
use matrix_sdk;
use serde;
use serde_json;
use tokio;
use xdg;

const APPNAME: &str = "matrix-journal";

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
	/// Specific room to watch, or omit to watch all rooms.
	#[arg(short = 'r', long)]
	room: Option<String>,

	/// Mark events read on receipt.
	#[arg(short = 'x', long)]
	acknowledge: bool,

	/// Output file for received events, or omit to write to standard output.
	#[arg(short = 'o', long)]
	out: Option<String>,

	/// Whether to write each event as a single-line JSON object, rather than the default plain text.
	#[arg(short = 'j', long)]
	json: bool,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct Settings {
	user: String,
	password: Option<String>, // Can be unset once session is populated.
	homeserver: String,
	db_key: String,
	session: Option<matrix_sdk::authentication::matrix::MatrixSession>,
	sync_token: Option<String>,
}

impl Settings {
	fn config_path() -> Result<std::path::PathBuf> {
		Ok(xdg::BaseDirectories::with_prefix(APPNAME).place_data_file("session.json")?)
	}

	fn db_path() -> Result<std::path::PathBuf> {
		Ok(xdg::BaseDirectories::with_prefix(APPNAME).create_data_directory("state")?)
	}

	fn load() -> Result<Self> {
		Ok(serde_json::from_reader(std::fs::File::open(&Self::config_path()?)?)?)
	}

	fn save(&self) -> Result<()> {
		// Write to a new file and rename, to avoid data loss if writing fails.
		let path = Self::config_path()?;
		let mut tmp_path = path.clone();
		tmp_path.add_extension("new");
		serde_json::to_writer(std::fs::File::create(&tmp_path)?, &self)?;
		std::fs::rename(&tmp_path, &path)?;
		Ok(())
	}

	fn update_sync_token(&mut self, token: String) -> Result<()> {
		self.sync_token = Some(token);
		self.save()
	}
}

enum IncomingEvent {
	Message(matrix_sdk::ruma::events::room::message::SyncRoomMessageEvent),
	Reaction(matrix_sdk::ruma::events::reaction::SyncReactionEvent),
	Sticker(matrix_sdk::ruma::events::sticker::SyncStickerEvent),
	Redaction(matrix_sdk::ruma::events::room::redaction::SyncRoomRedactionEvent),
}

impl IncomingEvent {
	fn kind(&self) -> &str {
		match self {
			Self::Message(_) => "message",
			Self::Reaction(_) => "reaction",
			Self::Sticker(_) => "sticker",
			Self::Redaction(_) => "redaction",
		}
	}

	fn id(&self) -> &matrix_sdk::ruma::EventId {
		match self {
			Self::Message(ev) => ev.event_id(),
			Self::Reaction(ev) => ev.event_id(),
			Self::Sticker(ev) => ev.event_id(),
			Self::Redaction(ev) => ev.event_id(),
		}
	}

	fn ts(&self) -> matrix_sdk::ruma::MilliSecondsSinceUnixEpoch {
		match self {
			Self::Message(ev) => ev.origin_server_ts(),
			Self::Reaction(ev) => ev.origin_server_ts(),
			Self::Sticker(ev) => ev.origin_server_ts(),
			Self::Redaction(ev) => ev.origin_server_ts(),
		}
	}

	fn sender(&self) -> &matrix_sdk::ruma::UserId {
		match self {
			Self::Message(ev) => ev.sender(),
			Self::Reaction(ev) => ev.sender(),
			Self::Sticker(ev) => ev.sender(),
			Self::Redaction(ev) => ev.sender(),
		}
	}

	fn body(&self) -> String {
		use matrix_sdk::ruma::events::reaction::*;
		use matrix_sdk::ruma::events::room::redaction::*;
		use matrix_sdk::ruma::events::room::message::*;
		use matrix_sdk::ruma::events::sticker::*;

		match self {
			Self::Message(ev) => {
				match ev {
					SyncRoomMessageEvent::Original(message) => message.content.msgtype.body().to_string(),
					SyncRoomMessageEvent::Redacted(_) => "(redacted)".to_string(),
				}
			},
			Self::Reaction(ev) => {
				match ev {
					SyncReactionEvent::Original(reaction) => reaction.content.relates_to.key.clone(),
					SyncReactionEvent::Redacted(_) => "(redacted)".to_string(),
				}
			},
			Self::Sticker(ev) => {
				match ev {
					SyncStickerEvent::Original(sticker) => sticker.content.body.to_string(),
					SyncStickerEvent::Redacted(_) => "(redacted)".to_string(),
				}
			},
			Self::Redaction(ev) => {
				match ev {
					SyncRoomRedactionEvent::Original(_) => "(redaction)".to_string(),
					SyncRoomRedactionEvent::Redacted(_) => "(redacted)".to_string(),
				}
			},
		}
	}

	fn url(&self) -> Option<String> {
		use matrix_sdk::ruma::events::room::message::*;
		use matrix_sdk::ruma::events::room::MediaSource;
		use matrix_sdk::ruma::events::sticker::*;

		match self {
			Self::Message(ev) => {
				let media = match ev {
					SyncRoomMessageEvent::Original(message) => match &message.content.msgtype {
						MessageType::Image(image) => Some(image.source.clone()),
						MessageType::Audio(audio) => Some(audio.source.clone()),
						MessageType::Video(video) => Some(video.source.clone()),
						MessageType::File(file) => Some(file.source.clone()),
						_ => None,
					},
					SyncRoomMessageEvent::Redacted(_) => None,
				};
				match media {
					Some(MediaSource::Plain(uri)) => Some(uri.as_str().to_string()),
					Some(MediaSource::Encrypted(enc)) => Some(enc.url.as_str().to_string()),
					_ => None,
				}
			},
			Self::Sticker(ev) => {
				match ev {
					SyncStickerEvent::Original(sticker) => match &sticker.content.source {
						StickerMediaSource::Plain(uri) => Some(uri.as_str().to_string()),
						StickerMediaSource::Encrypted(enc) => Some(enc.url.as_str().to_string()),
						_ => None,
					},
					SyncStickerEvent::Redacted(_) => None,
				}
			},
			Self::Reaction(_) => None,
			Self::Redaction(_) => None,
		}
	}
}

#[allow(unused)]
#[derive(serde::Serialize, PartialEq, Eq)]
struct Message {
	id: String,
	kind: String,
	ts: chrono::DateTime<chrono::Utc>,
	room: String,
	sender: String,
	body: String,
	url: Option<String>,
}

impl Message {
	fn tzstr(&self) -> String {
		// This is an unfortunate hack from https://github.com/chronotope/chrono/issues/960.
		use iana_time_zone;
		use chrono::TimeZone;
		use chrono_tz::OffsetName;
		let naive_tz = self.ts.format("%Z").to_string();
		match iana_time_zone::get_timezone().map(|x| x.parse::<chrono_tz::Tz>()) {
			Ok(Ok(tz)) => {
				let offset = tz.offset_from_utc_date(&self.ts.date_naive());
				offset.abbreviation().map(|x| x.to_string()).unwrap_or(naive_tz)
			},
			_ => naive_tz,
		}
	}
}

impl std::fmt::Display for Message {
	fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
		let local_ts: chrono::DateTime<chrono::Local> = chrono::DateTime::from(self.ts);
		let mut parts = vec![
			format!("{} {}", local_ts.format("%Y-%m-%d %H:%M:%S"), self.tzstr()),
			self.room.to_string(),
			self.sender.to_string(),
			self.body.to_string(),
		];
		if let Some(url) = &self.url { parts.push(url.to_string()); }
		f.write_str(&parts.join(" | "))?;
		Ok(())
	}
}

impl std::cmp::Ord for Message {
	fn cmp(&self, other: &Self) -> std::cmp::Ordering {
		self.ts.cmp(&other.ts)
	}
}

impl std::cmp::PartialOrd for Message {
	fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
		Some(self.ts.cmp(&other.ts))
	}
}

async fn login(settings: &mut Settings) -> Result<matrix_sdk::Client> {
	let user = matrix_sdk::ruma::UserId::parse(&settings.user)?;
	let client = matrix_sdk::Client::builder()
		.homeserver_url(&settings.homeserver)
		.sqlite_store(Settings::db_path()?, Some(&settings.db_key))
		.build().await?;
	if let Some(session) = &settings.session {
		client.restore_session(session.clone()).await?;
	}
	else if let Some(password) = &settings.password {
		client.matrix_auth().login_username(user, password).initial_device_display_name(APPNAME).await?;
		settings.session = Some(client.matrix_auth().session().expect("No session for logged-in client"));
		settings.save()?;
	}
	else {
		anyhow::bail!("No existing session, and no password set in the session file");
	}
	Ok(client)
}

async fn verify_device(request: matrix_sdk::encryption::verification::VerificationRequest) {
	use matrix_sdk::encryption::verification::*;
	use matrix_sdk::stream::StreamExt;
	request.accept().await.expect("Couldn't accept verification request");
	let mut stream = request.changes();
	while let Some(request_state) = stream.next().await {
		match request_state {
			VerificationRequestState::Transitioned { verification } => {
				if let Verification::SasV1(sas) = verification {
					sas.accept().await.expect("Couldn't accept SAS verification");
					let mut stream = sas.changes();
					while let Some(sas_state) = stream.next().await {
						match sas_state {
							SasState::KeysExchanged { emojis, .. } => {
								let emojistr = emojis.map(|x| x.emojis.map(|c| c.symbol).join(" ")).unwrap_or("(not provided)".to_string());
								log::info!("Verification keys exchanged: {}", emojistr);
								if let Err(e) = sas.confirm().await {
									log::error!("Confirming verification failed: {}", e);
								}
							},
							SasState::Cancelled(cancel) => {
								log::info!("Verification canceled: {}", cancel.reason());
								break;
							},
							_ => (),
						}
					}
					break;
				}
			}
			VerificationRequestState::Done => log::info!("Successfully verified."),
			VerificationRequestState::Cancelled(cancel) => log::info!("Verification canceled: {}", cancel.reason()),
			_ => (),
		}
	}
}

fn write_out(msg: Message, json: bool, out: &std::sync::Arc<std::sync::Mutex<Box<dyn std::io::Write + std::marker::Send + std::marker::Sync>>>) {
	let content = match json {
		true => serde_json::to_string(&msg).expect("Serialize failed"),
		false => msg.to_string(),
	};
	if let Err(e) = writeln!(&mut out.lock().expect("Panic while holding output lock"), "{}", content) {
		log::error!("Failed to write output: {}", e);
	}
}

// Other events to consider watching:
// https://docs.rs/matrix-sdk/latest/matrix_sdk/ruma/events/typing/type.TypingEvent.html
// https://docs.rs/matrix-sdk/latest/matrix_sdk/ruma/events/presence/struct.PresenceEvent.html

#[tokio::main]
async fn main() -> Result<()> {
	env_logger::init_from_env(env_logger::Env::default().filter_or("RUST_LOG", "info"));

	let args = Args::parse();
	let raw_out: Box<dyn std::io::Write + Send + Sync> = match args.out {
		Some(outfile) => Box::new(std::fs::File::options().create(true).append(true).open(outfile)?),
		None => Box::new(std::io::stdout()),
	};
	let out = std::sync::Arc::new(std::sync::Mutex::new(raw_out));
	let mut settings = Settings::load()?;
	log::info!("Logging in as {}...", settings.user);
	let client = login(&mut settings).await?;

	let mut filter = matrix_sdk::ruma::api::client::filter::FilterDefinition::with_lazy_loading();
	if let Some(room) = args.room {
		let target_room = matrix_sdk::ruma::RoomId::parse(room)?;
		filter.room.rooms = Some(vec![target_room]);
	}
	let mut sync_settings = matrix_sdk::config::SyncSettings::default().filter(filter.into());
	let settings = std::sync::Arc::new(std::sync::Mutex::new(settings));

	let sort_queue = std::sync::Arc::new(std::sync::Mutex::new(std::collections::BinaryHeap::new()));
	let should_enqueue = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));

	let handle_event = {
		let sort_queue = sort_queue.clone();
		let should_enqueue = should_enqueue.clone();
		let out = out.clone();
		async move |ev: IncomingEvent, room: &matrix_sdk::Room| {
			if args.acknowledge {
				if let Err(e) = room.send_single_receipt(matrix_sdk::ruma::api::client::receipt::create_receipt::v3::ReceiptType::Read, matrix_sdk::ruma::events::receipt::ReceiptThread::Unthreaded, ev.id().into()).await {
					log::error!("Failed to update read markers: {}", e);
				}
			}

			let msg = Message {
				id: ev.id().to_string(),
				kind: ev.kind().to_string(),
				ts: chrono::DateTime::from(ev.ts().to_system_time().expect("Couldn't convert timestamp to system time")),
				room: room.display_name().await.map(|x| x.to_string()).unwrap_or(room.room_id().to_string()),
				sender: room.get_member(ev.sender()).await.unwrap_or(None).map(|x| x.name().to_string()).unwrap_or("(unknown)".to_string()),
				body: ev.body(),
				url: ev.url(),
			};
			match should_enqueue.load(std::sync::atomic::Ordering::Relaxed) {
				true => sort_queue.lock().expect("Panic while holding sort queue lock").push(std::cmp::Reverse(msg)),
				false => write_out(msg, args.json, &out),
			};
		}
	};

	// Room events.
	let handle_ev = handle_event.clone(); // Ugh, can we do better?
	client.add_event_handler(|ev: matrix_sdk::ruma::events::room::message::SyncRoomMessageEvent, room: matrix_sdk::Room| async move {
		handle_ev(IncomingEvent::Message(ev), &room).await;
	});
	let handle_ev = handle_event.clone();
	client.add_event_handler(|ev: matrix_sdk::ruma::events::reaction::SyncReactionEvent, room: matrix_sdk::Room| async move {
		handle_ev(IncomingEvent::Reaction(ev), &room).await;
	});
	let handle_ev = handle_event.clone();
	client.add_event_handler(|ev: matrix_sdk::ruma::events::sticker::SyncStickerEvent, room: matrix_sdk::Room| async move {
		handle_ev(IncomingEvent::Sticker(ev), &room).await;
	});
	let handle_ev = handle_event.clone();
	client.add_event_handler(|ev: matrix_sdk::ruma::events::room::redaction::SyncRoomRedactionEvent, room: matrix_sdk::Room| async move {
		handle_ev(IncomingEvent::Redaction(ev), &room).await;
	});

	// Key verification requests.
	client.add_event_handler(|ev: matrix_sdk::ruma::events::key::verification::request::ToDeviceKeyVerificationRequestEvent, client: matrix_sdk::Client| async move {
		let request = client.encryption().get_verification_request(&ev.sender, &ev.content.transaction_id).await.expect("Request object wasn't created");
		tokio::spawn(verify_device(request));
	});

	loop {
		if let Some(ref token) = settings.lock().expect("Panic while holding settings lock").sync_token {
			sync_settings = sync_settings.token(token.clone());
		}
		// Fetch any messages that arrived while we were not listening, and sort them before printing.
		should_enqueue.store(true, std::sync::atomic::Ordering::Relaxed);
		loop {
			if let Ok(response) = client.sync_once(sync_settings.clone()).await {
				sync_settings = sync_settings.token(response.next_batch.clone());
				settings.lock().expect("Panic while holding settings lock").update_sync_token(response.next_batch)?;
				break;
			}
		}
		should_enqueue.store(false, std::sync::atomic::Ordering::Relaxed);
		let mut locked_queue = sort_queue.lock().expect("Panic while holding sort queue lock");
		while let Some(msg) = locked_queue.pop() { write_out(msg.0, args.json, &out); }
		log::info!("Synced to latest room state.");

		// Listen for messages until we hit an error.
		let res = client.sync_with_result_callback(sync_settings.clone(), |sync_result| {
			let settings = settings.clone();
			async move {
				let response = sync_result?;
				settings.lock()
					.expect("Panic while holding settings lock")
					.update_sync_token(response.next_batch)
					.map_err(|err| matrix_sdk::Error::UnknownError(err.into()))?;
				Ok(matrix_sdk::LoopCtrl::Continue)
			}
		}).await;
		match res {
			Ok(_) => break,
			Err(e) => log::warn!("Reconnecting: {}", e),
		};
	}
	Ok(())
}
