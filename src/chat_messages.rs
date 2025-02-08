/*!
 * Processing of chat messages
 */

use affinidi_messaging_didcomm::Message;
use affinidi_messaging_sdk::{
    messages::known::MessageType, profiles::Profile,
    protocols::message_pickup::MessagePickupStatusReply, ATM,
};
use anyhow::Result;
use console::style;
use ollama_rs::{generation::completion::request::GenerationRequest, Ollama};
use serde::{Deserialize, Serialize};
use sha256::digest;
use std::{
    str::FromStr,
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::{
    io::{stdout, AsyncWriteExt},
    select,
    sync::Mutex,
    time::Instant,
};
use tokio_stream::StreamExt;
use tracing::{error, info, warn};

use crate::{
    agents::state_management::{ChannelState, ChatChannelState},
    didcomm_messages::{handle_presence, oob_connection::send_connection_response},
};

#[derive(Deserialize, Serialize)]
struct ChatMessage {
    pub text: String,
}

#[derive(Deserialize, Serialize)]
struct ChatEffect {
    pub effect: String,
}

/// Processes a received message
/// Doesn't return anything
pub(crate) async fn handle_message<T>(
    atm: &ATM,
    profile: &Arc<Profile>,
    model: &Arc<Mutex<T>>,
    model_name: &str,
    message: &Message,
) -> Result<()>
where
    T: ChannelState,
{
    let Ok(msg_type) = MessageType::from_str(&message.type_) else {
        println!(
            "{}",
            style(format!("Unknown message type: {:?}", message)).red()
        );
        return Err(anyhow::anyhow!("Unknown message type"));
    };

    match msg_type {
        MessageType::MessagePickupStatusResponse => {
            match serde_json::from_value::<MessagePickupStatusReply>(message.body.clone()) {
                Ok(status) => {
                    println!(
                        "{}",
                        style(format!(
                            "STATUS: queued messages ({}), live_streaming?({})",
                            status.message_count, status.live_delivery
                        ))
                        .green()
                    );
                }
                Err(e) => {
                    println!(
                        "{}",
                        style(format!("Error parsing message body: {:?}", e)).red()
                    );
                    return Err(anyhow::anyhow!("Error parsing message body"));
                }
            }
        }
        MessageType::Other(_type) => match _type.as_str() {
            "https://affinidi.com/atm/client-actions/connection-setup" => {
                info!(
                    "{}: Received Connection Setup Request: from({:#?})",
                    profile.inner.alias, message.from
                );
                let new_did = send_connection_response(atm, profile, message).await?;
                {
                    let mut lock = model.lock().await;
                    let Some(from_did) = &message.from else {
                        println!("{}", style("No 'from' field in message").red());
                        println!(
                            "{}",
                            style("How would one respond to an anonymous message?").red()
                        );
                        return Err(anyhow::anyhow!("No 'from' field in message"));
                    };
                    let from_did_hash = digest(from_did);
                    lock.remove_channel_state(&from_did_hash);
                    let new_did_hash = digest(&new_did);
                    lock.insert_channel_state(
                        &new_did_hash,
                        ChatChannelState {
                            remote_did: new_did.clone(),
                            remote_did_hash: new_did_hash.clone(),
                            ..Default::default()
                        },
                    );
                }
                let _ = send_message(
                    atm,
                    profile,
                    &format!(
                        "First Message from a very intelligent {}",
                        profile.inner.alias
                    ),
                    &new_did,
                    model,
                )
                .await;
            }
            "https://affinidi.com/atm/client-actions/chat-presence" => {
                // Send a presence response back
                handle_presence(atm, profile, message).await;
            }
            "https://affinidi.com/atm/client-actions/chat-effect" => {
                // Special handling for balloons and confetti
                handle_chat_effect(atm, profile, model, message).await;
            }
            "https://affinidi.com/atm/client-actions/chat-message" => {
                let _ = ack_message(atm, profile, message).await;
                match serde_json::from_value::<ChatMessage>(message.body.clone()) {
                    Ok(chat_message) => {
                        println!(
                            "{}",
                            style(format!(
                                "Model ({}): incoming prompt: {:?}",
                                model_name, chat_message.text
                            ))
                            .green()
                        );
                        if chat_message.text.starts_with("/") {
                            let _ = handle_command(
                                atm,
                                profile,
                                &chat_message,
                                model,
                                message.from.as_ref().unwrap(),
                            )
                            .await;
                        } else {
                            let _ = handle_prompt(
                                atm,
                                profile,
                                &chat_message,
                                model,
                                message.from.as_ref().unwrap(),
                            )
                            .await;
                        }
                    }
                    Err(e) => {
                        println!(
                            "{}",
                            style(format!("Error parsing chat message: {:?}", e)).red()
                        );
                        return Err(anyhow::anyhow!("Error parsing chat message"));
                    }
                }
            }
            "https://affinidi.com/atm/client-actions/chat-delivered" => {
                // Ignore this, it is the other client acknowledging receipt of a message
            }
            "https://affinidi.com/atm/client-actions/chat-activity" => {
                // Ignore this, other client is typing
            }
            _ => {
                println!(
                    "{}\n{}",
                    style(format!("Unknown Message Type: {} received!", _type)).red(),
                    style(format!("Message: {:?}", message)).cyan()
                );
            }
        },
        _ => {
            println!("Received message: {:?}", message);
        }
    }
    Ok(())
}

pub(crate) async fn handle_chat_effect<T>(
    atm: &ATM,
    profile: &Arc<Profile>,
    model: &Arc<Mutex<T>>,
    message: &Message,
) where
    T: ChannelState,
{
    match serde_json::from_value::<ChatEffect>(message.body.clone()) {
        Ok(chat_effect) => {
            println!(
                "{}",
                style(format!(
                    "Model ({}): incoming effect: {:?}",
                    profile.inner.alias, chat_effect.effect
                ))
                .green()
            );
            let prompt = if chat_effect.effect == "balloons" {
                "I give you a balloon".to_string()
            } else if chat_effect.effect == "confetti" {
                "Let's celebrate".to_string()
            } else {
                "I don't know what to do with this".to_string()
            };
            let _ = handle_prompt(
                atm,
                profile,
                &ChatMessage { text: prompt },
                model,
                message.from.as_ref().unwrap(),
            )
            .await;
        }
        Err(e) => {
            println!(
                "{}",
                style(format!("Error parsing chat message: {:?}", e)).red()
            );
        }
    }
}

/// Handles a command message
async fn handle_command<T>(
    atm: &ATM,
    profile: &Arc<Profile>,
    chat_message: &ChatMessage,
    model: &Arc<Mutex<T>>,
    remote_did: &str,
) -> Result<()>
where
    T: ChannelState,
{
    let response = if chat_message.text.to_lowercase() == "/help" {
        r#"Help:
          /help - Display this help message
          /think - Status of the think tokens being displayed
          /think on|off - Turn think tokens on or off
          /dids - Display the DID's for this chat
        "#
        .to_string()
    } else if chat_message.text.to_lowercase() == "/dids" {
        format!(
            "DIDs:\nAgent: {}\nClient: {}",
            profile.inner.did, remote_did
        )
    } else {
        format!(
            "ERROR: unknown command: {}\nUse /help to show commands",
            chat_message.text
        )
    };

    let _ = send_message(atm, profile, &response, remote_did, model).await;

    Ok(())
}

/// Handles a prompt message
async fn handle_prompt<T>(
    atm: &ATM,
    profile: &Arc<Profile>,
    chat_message: &ChatMessage,
    model: &Arc<Mutex<T>>,
    to_did: &str,
) -> Result<()>
where
    T: ChannelState,
{
    let (ollama_host, ollama_port, model_name) = {
        let lock = model.lock().await;

        let model = lock.get_model().unwrap();

        (
            model.ollama_host.clone(),
            model.ollama_port,
            model.name.clone(),
        )
    };

    // Instantiate Ollama
    let ollama = Ollama::new(&ollama_host, ollama_port);

    let mut stream = ollama
        .generate_stream(GenerationRequest::new(
            model_name.clone(),
            chat_message.text.clone(),
        ))
        .await
        .unwrap();

    let mut stdout = stdout();
    stdout.write_all(b"\n> ").await?;
    stdout.flush().await?;

    let mut think_flag = true;
    let mut output = String::new();

    let timeout: tokio::time::Sleep = tokio::time::sleep(Duration::from_secs(30));
    let mut typing_interval = tokio::time::interval_at(
        Instant::now() + Duration::from_secs(3),
        Duration::from_secs(3),
    );
    tokio::pin!(timeout);

    let _ = i_am_thinking(atm, profile, model, to_did).await;
    loop {
        select! {
            _ = &mut timeout => {
                warn!("AI Response timed out");
                let _ = send_message(atm, profile, "Timeout: I'm sorry, I'm taking too long to respond", to_did, model).await;
                break;
            }
            _ = typing_interval.tick() => {
                let _ = i_am_thinking(atm, profile, model, to_did).await;
            }
            token = stream.next() => {
                match token {
                    Some(Ok(res)) => {
                        for ele in res {
                            //stdout.write_all(ele.response.as_bytes()).await?;
                            if !think_flag {
                                if ele.response == "\n\n" {
                                    continue;
                                } else if ele.response == ".\n\n" {
                                    output.push_str(&ele.response);
                                    let _ = send_message(atm, profile, &output, to_did, model).await;
                                    output.clear();

                                    continue;
                                }
                                //println!("{:?}", ele);
                                output.push_str(&ele.response);
                            }
                            if ele.response.contains("</think>") {
                                think_flag = false;
                            }

                            stdout.flush().await?;
                        }
                    }
                    Some(Err(err)) => {
                        error!("Error: {:?}", err);
                        break;
                    }
                    None => {
                        break;
                    }
                }
            }
        }
    }

    let _ = send_message(atm, profile, &output, to_did, model).await;
    println!("{}", style("AI Responded...").cyan());

    Ok(())
}

pub async fn send_message<T>(
    atm: &ATM,
    profile: &Arc<Profile>,
    text: &str,
    to_did: &str,
    channel_state: &Arc<Mutex<T>>,
) -> Result<()>
where
    T: ChannelState,
{
    let seq_no = {
        let mut channel_state = channel_state.lock().await;
        let state = channel_state
            .get_channel_state_mut(&digest(to_did))
            .unwrap();
        let seq_no = state.seq_no;
        state.seq_no += 1;

        seq_no
    };
    let id = uuid::Uuid::new_v4().to_string();
    let msg = Message::build(
        id.clone(),
        "https://affinidi.com/atm/client-actions/chat-message".to_string(),
        serde_json::json!({ "text": text, "seqNo": seq_no }),
    )
    .created_time(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .from(profile.inner.did.clone())
    .to(to_did.to_string())
    .finalize();

    let packed = atm
        .pack_encrypted(
            &msg,
            to_did,
            Some(&profile.inner.did),
            Some(&profile.inner.did),
        )
        .await?;

    if packed.1.messaging_service.is_none() {
        let _ = atm
            .forward_and_send_message(
                profile,
                &packed.0,
                None,
                profile.dids()?.1,
                to_did,
                None,
                None,
                false,
            )
            .await?;
    } else {
        let _ = atm
            .send_message(profile, &packed.0, &id, false, false)
            .await?;
    }
    Ok(())
}

async fn ack_message(atm: &ATM, profile: &Arc<Profile>, message: &Message) -> Result<()> {
    let Some(from_did) = message.from.clone() else {
        println!("{}", style("No 'from' field in message").red());
        println!(
            "{}",
            style("How would one respond to an anonymous message?").red()
        );
        return Err(anyhow::anyhow!("No 'from' field in message"));
    };

    let id = uuid::Uuid::new_v4().to_string();
    let new_msg = Message::build(
        id.clone(),
        "https://affinidi.com/atm/client-actions/chat-delivered".to_string(),
        serde_json::json!({ "messages": vec![message.id.to_string()] }),
    )
    .created_time(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .from(profile.inner.did.clone())
    .to(from_did.to_string())
    .finalize();

    let packed = atm
        .pack_encrypted(
            &new_msg,
            &from_did,
            Some(&profile.inner.did),
            Some(&profile.inner.did),
        )
        .await?;

    if packed.1.messaging_service.is_none() {
        let _ = atm
            .forward_and_send_message(
                profile,
                &packed.0,
                None,
                profile.dids()?.1,
                &from_did,
                None,
                None,
                false,
            )
            .await?;
    } else {
        let _ = atm
            .send_message(profile, &packed.0, &id, false, false)
            .await?;
    }
    Ok(())
}

async fn i_am_thinking<T>(
    atm: &ATM,
    profile: &Arc<Profile>,
    channel_state: &Arc<Mutex<T>>,
    to_did: &str,
) -> Result<()>
where
    T: ChannelState,
{
    let activity_seq_no = {
        let mut channel_state = channel_state.lock().await;
        let state = channel_state
            .get_channel_state_mut(&digest(to_did))
            .unwrap();
        let activity_seq_no = state.activity_seq_no;
        state.activity_seq_no += 1;

        activity_seq_no
    };
    let id = uuid::Uuid::new_v4().to_string();
    let new_msg = Message::build(
        id.clone(),
        "https://affinidi.com/atm/client-actions/chat-activity".to_string(),
        serde_json::json!({ "activitySeqNo": activity_seq_no }),
    )
    .created_time(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .from(profile.inner.did.clone())
    .to(to_did.to_string())
    .finalize();

    println!("{}", style("Typing...").cyan());

    let packed = atm
        .pack_encrypted(
            &new_msg,
            to_did,
            Some(&profile.inner.did),
            Some(&profile.inner.did),
        )
        .await?;

    if packed.1.messaging_service.is_none() {
        let _ = atm
            .forward_and_send_message(
                profile,
                &packed.0,
                None,
                profile.dids()?.1,
                to_did,
                Some(
                    SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_secs()
                        + 10,
                ),
                None,
                false,
            )
            .await?;
    } else {
        let _ = atm
            .send_message(profile, &packed.0, &id, false, false)
            .await?;
    }
    Ok(())
}
