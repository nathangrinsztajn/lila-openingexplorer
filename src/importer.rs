use std::sync::Arc;

use rustc_hash::FxHashMap;
use serde::Deserialize;
use serde_with::{serde_as, DisplayFromStr, SpaceSeparator, StringWithSeparator};
use shakmaty::{
    fen::Fen,
    san::San,
    uci::Uci,
    variant::{Variant, VariantPosition},
    zobrist::Zobrist,
    ByColor, CastlingMode, Chess, Color, Outcome, Position,
};
use tokio::sync::Mutex;

use crate::{
    api::{Error, LilaVariant},
    db::Database,
    model::{
        GameId, GamePlayer, Key, KeyBuilder, LaxDate, LichessEntry, LichessGame, MastersEntry,
        MastersGameWithId, Mode, Speed, Year,
    },
    util::ByColorDef,
};

const MAX_PLIES: usize = 40;

#[derive(Clone)]
pub struct MastersImporter {
    db: Arc<Database>,
    mutex: Arc<Mutex<()>>,
}

impl MastersImporter {
    pub fn new(db: Arc<Database>) -> MastersImporter {
        MastersImporter {
            db,
            mutex: Arc::new(Mutex::new(())),
        }
    }

    pub async fn import(&self, body: MastersGameWithId) -> Result<(), Error> {
        if body.game.players.white.rating / 2 + body.game.players.black.rating / 2 < 2200 {
            return Err(Error::RejectedImport(body.id));
        }

        let year = body.game.date.year();
        if year < Year::min_masters() || Year::max_masters() < year {
            return Err(Error::RejectedImport(body.id));
        }

        let _guard = self.mutex.lock();
        let masters_db = self.db.masters();
        if masters_db
            .has_game(body.id)
            .expect("check for masters game")
        {
            return Err(Error::DuplicateGame(body.id));
        }

        let mut without_loops: FxHashMap<Key, (Uci, Color)> =
            FxHashMap::with_capacity_and_hasher(body.game.moves.len(), Default::default());
        let mut pos: Zobrist<Chess, u128> = Zobrist::default();
        let mut final_key = None;
        for uci in &body.game.moves {
            let key = KeyBuilder::masters()
                .with_zobrist(Variant::Chess, pos.zobrist_hash())
                .with_year(year);
            final_key = Some(key.clone());
            let m = uci.to_move(&pos)?;
            without_loops.insert(key, (Uci::from_chess960(&m), pos.turn()));
            pos.play_unchecked(&m);
        }

        if let Some(final_key) = final_key {
            if masters_db.has(final_key).expect("check for masters entry") {
                return Err(Error::DuplicateGame(body.id));
            }
        }

        let mut batch = masters_db.batch();
        batch.put_game(body.id, &body.game);
        for (key, (uci, turn)) in without_loops {
            batch.merge(
                key,
                MastersEntry::new_single(
                    uci,
                    body.id,
                    Outcome::from_winner(body.game.winner),
                    body.game.players.get(turn).rating,
                    body.game.players.get(!turn).rating,
                ),
            );
        }

        batch.commit().expect("commit masters game");
        Ok(())
    }
}

#[serde_as]
#[derive(Deserialize)]
pub struct LichessGameImport {
    variant: Option<LilaVariant>,
    speed: Speed,
    #[serde_as(as = "Option<DisplayFromStr>")]
    fen: Option<Fen>,
    #[serde_as(as = "DisplayFromStr")]
    id: GameId,
    #[serde_as(as = "DisplayFromStr")]
    date: LaxDate,
    #[serde(flatten, with = "ByColorDef")]
    players: ByColor<GamePlayer>,
    #[serde_as(as = "Option<DisplayFromStr>")]
    winner: Option<Color>,
    #[serde_as(as = "StringWithSeparator<SpaceSeparator, San>")]
    moves: Vec<San>,
}

#[derive(Clone)]
pub struct LichessImporter {
    db: Arc<Database>,
    mutex: Arc<Mutex<()>>,
}

impl LichessImporter {
    pub fn new(db: Arc<Database>) -> LichessImporter {
        LichessImporter {
            db,
            mutex: Arc::new(Mutex::new(())),
        }
    }

    pub async fn import(&self, game: LichessGameImport) -> Result<(), Error> {
        let _guard = self.mutex.lock();

        let lichess_db = self.db.lichess();

        if lichess_db
            .game(game.id)
            .expect("get game info")
            .map_or(false, |info| info.indexed_lichess)
        {
            log::debug!("lichess game {} already imported", game.id);
            return Ok(());
        }

        if game.speed == Speed::Bullet {
            // log::debug!("lichess game is a fucking bullet");
            return Ok(());
        }

        if game.speed == Speed::UltraBullet {
            return Ok(());
        }

        let month = match game.date.month() {
            Some(month) => month,
            None => {
                log::error!("lichess game {} missing month", game.id);
                return Err(Error::RejectedImport(game.id));
            }
        };
        let outcome = Outcome::from_winner(game.winner);
        let variant = Variant::from(game.variant.unwrap_or_default());

        let mut pos: Zobrist<_, u128> = Zobrist::new(match game.fen {
            Some(fen) => {
                VariantPosition::from_setup(variant, fen.into_setup(), CastlingMode::Chess960)?
            }
            None => VariantPosition::new(variant),
        });

        let mut without_loops: FxHashMap<Key, (Uci, Color)> =
            FxHashMap::with_capacity_and_hasher(game.moves.len(), Default::default());
        for (ply, san) in game.moves.into_iter().enumerate() {
            if ply >= MAX_PLIES {
                break;
            }

            let m = san.to_move(&pos)?;
            without_loops.insert(
                KeyBuilder::lichess()
                    .with_zobrist(variant, pos.zobrist_hash())
                    .with_month(month),
                (Uci::from_chess960(&m), pos.turn()),
            );
            pos.play_unchecked(&m);
        }

        let mut batch = lichess_db.batch();
        batch.merge_game(
            game.id,
            LichessGame {
                mode: Mode::Rated,
                indexed_player: Default::default(),
                indexed_lichess: true,
                outcome,
                players: game.players.clone(),
                month,
                speed: game.speed,
            },
        );
        for (key, (uci, turn)) in without_loops {
            batch.merge_lichess(
                key,
                LichessEntry::new_single(
                    uci,
                    game.speed,
                    game.id,
                    outcome,
                    game.players.get(turn).rating,
                    game.players.get(!turn).rating,
                ),
            );
        }

        batch.commit().expect("commit lichess game");
        Ok(())
    }
}
