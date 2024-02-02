// Copyright 2023 Lukas Werling

// Permission to use, copy, modify, and/or distribute this software for any
// purpose with or without fee is hereby granted, provided that the above
// copyright notice and this permission notice appear in all copies.

// THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
// WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
// MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
// ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
// ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
// OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

use anyhow::Context;
use axum::{
    extract::State,
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::get,
    Router,
};
use futures_util::stream::{self, Stream, StreamExt};
use itertools::Itertools;
use log::{error, info, warn, LevelFilter};
use redis::{AsyncCommands, RedisError};
use std::{convert::Infallible, sync::Arc};
use systemd_journal_logger::{connected_to_journal, JournalLog};

struct AppState {
    redis: redis::Client,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if connected_to_journal() {
        JournalLog::new()?.install()?;
        log::set_max_level(LevelFilter::Info);
    } else {
        simple_logger::init_with_env()?;
    }

    let redis_url = std::env::var("REDIS_URL")?;
    let listen_url = &std::env::var("LISTEN_URL")?
        .parse()
        .context("invalid LISTEN_URL")?;

    let redis_client = redis::Client::open(redis_url).context("redis open failed")?;
    let redis_conn_info = redis_client.get_connection_info().clone();
    let state = Arc::new(AppState {
        redis: redis_client,
    });

    let app = Router::new()
        .route("/game_events", get(game_events))
        .with_state(state);

    info!("Listening on {}", listen_url);
    info!("Redis connection: {:?}", redis_conn_info);

    axum::Server::bind(listen_url)
        .serve(app.into_make_service())
        .await?;

    Ok(())
}

/// Get init message with array of games.
async fn games_init(redis_client: &redis::Client) -> Result<impl Stream<Item = Event>, AppError> {
    let mut redis_conn = redis_client.get_async_connection().await?;
    let active_game_ids: Vec<String> = redis_conn
        .smembers("league:active_games")
        .await
        .context("smembers league:active_games")?;
    let active_games: Vec<Option<String>> = if active_game_ids.is_empty() {
        Vec::new()
    } else {
        redis_conn
            .get(
                active_game_ids
                    .iter()
                    .map(|id| format!("league:game:{}", id))
                    .collect::<Vec<String>>(),
            )
            .await
            .context("mget <active game ids>")?
    };

    // Remove expired games from the set.
    for (id, game) in active_game_ids.iter().zip(active_games.iter()) {
        if game.is_none() {
            warn!("Removing expired league:active_games {}", id);
            redis_conn
                .srem("league:active_games", id)
                .await
                .context("srem league:active_games")?;
        }
    }

    let init_data = format!(
        "[{}]",
        active_games.iter().filter_map(|g| g.as_ref()).join(",")
    );
    Ok(stream::once(async {
        Event::default().event("init").data(init_data)
    }))
}

/// Handler for /game_events
async fn game_events(
    State(state): State<Arc<AppState>>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    let mut pubsub = state.redis.get_async_connection().await?.into_pubsub();

    let stream_init = games_init(&state.redis).await?;

    pubsub
        .psubscribe("league:game:*")
        .await
        .context("psubscribe league:game:*")?;
    let stream_msg = pubsub
        .into_on_message()
        .then(move |msg| {
            // Skip league:game:
            let event = msg.get_channel_name()[12..].to_string();
            let game_id: Result<String, RedisError> = msg.get_payload();
            let state = state.clone();
            async move {
                let mut redis_conn = state.redis.get_async_connection().await?;
                let game: String = redis_conn.get(format!("league:game:{}", game_id?)).await?;
                anyhow::Result::<_>::Ok(Event::default().event(event).data(game))
            }
        })
        .map(|res| match res {
            Ok(event) => event,
            Err(err) => {
                error!("{:#}", err);
                Event::default().event("error").data(format!("{:#}", err))
            }
        });

    Ok(Sse::new(stream::select(stream_init, stream_msg).map(Ok)).keep_alive(KeepAlive::default()))
}

// Make our own error that wraps `anyhow::Error`.
struct AppError(anyhow::Error);

// Tell axum how to convert `AppError` into a response.
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        error!("{:#}", self.0);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Something went wrong: {:#}", self.0),
        )
            .into_response()
    }
}

// This enables using `?` on functions that return `Result<_, anyhow::Error>` to turn them into
// `Result<_, AppError>`. That way you don't need to do that manually.
impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}
