use std::{
    cmp::{max, min, Reverse},
    fmt,
    io::{self, Read, Write},
    time::{Duration, SystemTime},
};

use byteorder::{ReadBytesExt as _, WriteBytesExt as _};
use rustc_hash::FxHashMap;
use shakmaty::{uci::Uci, Outcome};
use smallvec::{smallvec, SmallVec};

use crate::{
    api::PlayerQueryFilter,
    model::{
        read_uci, read_uint, write_uci, write_uint, ByMode, BySpeed, GameId, LichessGroup, Mode,
        PreparedMove, PreparedResponse, Speed, Stats,
    },
};

const MAX_PLAYER_GAMES: usize = 8; // must fit into 4 bits

#[derive(Debug, Eq, PartialEq)]
enum Header {
    Group {
        mode: Mode,
        speed: Speed,
        num_games: usize,
    },
    End,
}

impl Header {
    fn read<R: Read>(reader: &mut R) -> io::Result<Header> {
        let n = reader.read_u8()?;
        Ok(Header::Group {
            speed: match n & 7 {
                0 => return Ok(Header::End),
                1 => Speed::UltraBullet,
                2 => Speed::Bullet,
                3 => Speed::Blitz,
                4 => Speed::Rapid,
                5 => Speed::Classical,
                6 => Speed::Correspondence,
                _ => return Err(io::ErrorKind::InvalidData.into()),
            },
            mode: Mode::from_rated((n >> 3) & 1 == 1),
            num_games: usize::from(n >> 4),
        })
    }

    fn write<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        writer.write_u8(match *self {
            Header::End => 0,
            Header::Group {
                mode,
                speed,
                num_games,
            } => {
                (match speed {
                    Speed::UltraBullet => 1,
                    Speed::Bullet => 2,
                    Speed::Blitz => 3,
                    Speed::Rapid => 4,
                    Speed::Classical => 5,
                    Speed::Correspondence => 6,
                }) | ((mode.is_rated() as u8) << 3)
                    | ((num_games as u8) << 4)
            }
        })
    }
}

#[derive(Default, Debug)]
pub struct PlayerEntry {
    sub_entries: FxHashMap<Uci, BySpeed<ByMode<LichessGroup>>>,
    max_game_idx: Option<u64>,
}

impl PlayerEntry {
    pub const SIZE_HINT: usize = 13;

    pub fn new_single(
        uci: Uci,
        speed: Speed,
        mode: Mode,
        game_id: GameId,
        outcome: Outcome,
        opponent_rating: u16,
    ) -> PlayerEntry {
        let mut sub_entry: BySpeed<ByMode<LichessGroup>> = Default::default();
        *sub_entry.by_speed_mut(speed).by_mode_mut(mode) = LichessGroup {
            stats: Stats::new_single(outcome, opponent_rating),
            games: smallvec![(0, game_id)],
        };
        let mut sub_entries = FxHashMap::with_capacity_and_hasher(1, Default::default());
        sub_entries.insert(uci, sub_entry);

        PlayerEntry {
            sub_entries,
            max_game_idx: Some(0),
        }
    }

    pub fn extend_from_reader<R: Read>(&mut self, reader: &mut R) -> io::Result<()> {
        let base_game_idx = self.max_game_idx.map_or(0, |idx| idx + 1);

        loop {
            let uci = match read_uci(reader) {
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(err) => return Err(err),
                Ok(uci) => uci,
            };

            let sub_entry = self.sub_entries.entry(uci).or_default();

            loop {
                match Header::read(reader) {
                    Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                    Err(err) => return Err(err),
                    Ok(Header::End) => break,
                    Ok(Header::Group {
                        speed,
                        mode,
                        num_games,
                    }) => {
                        let stats = Stats::read(reader)?;
                        let mut games = SmallVec::with_capacity(num_games);
                        for _ in 0..num_games {
                            let game_idx = base_game_idx + read_uint(reader)?;
                            self.max_game_idx = Some(max(self.max_game_idx.unwrap_or(0), game_idx));
                            let game = GameId::read(reader)?;
                            games.push((game_idx, game));
                        }
                        let group = sub_entry.by_speed_mut(speed).by_mode_mut(mode);
                        *group += LichessGroup { stats, games };
                    }
                }
            }
        }
    }

    pub fn write<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        for (i, (uci, sub_entry)) in self.sub_entries.iter().enumerate() {
            if i > 0 {
                Header::End.write(writer)?;
            }

            write_uci(writer, uci)?;

            sub_entry.as_ref().try_map(|speed, by_mode| {
                by_mode.as_ref().try_map(|mode, group| {
                    if !group.games.is_empty() || !group.stats.is_empty() {
                        Header::Group {
                            speed,
                            mode,
                            num_games: min(group.games.len(), MAX_PLAYER_GAMES),
                        }
                        .write(writer)?;

                        group.stats.write(writer)?;

                        for (game_idx, game) in group
                            .games
                            .iter()
                            .skip(group.games.len().saturating_sub(MAX_PLAYER_GAMES))
                        {
                            write_uint(writer, *game_idx)?;
                            game.write(writer)?;
                        }
                    }

                    Ok::<_, io::Error>(())
                })
            })?;
        }

        Ok(())
    }

    pub fn prepare(self, filter: &PlayerQueryFilter) -> PreparedResponse {
        let mut total = Stats::default();
        let mut moves = Vec::with_capacity(self.sub_entries.len());
        let mut recent_games: Vec<(u64, Uci, GameId)> = Vec::new();

        for (uci, sub_entry) in self.sub_entries {
            let mut latest_game: Option<(u64, GameId)> = None;
            let mut stats = Stats::default();

            for speed in Speed::ALL {
                if filter
                    .speeds
                    .as_ref()
                    .map_or(true, |speeds| speeds.contains(&speed))
                {
                    for mode in Mode::ALL {
                        if filter
                            .modes
                            .as_ref()
                            .map_or(true, |modes| modes.contains(&mode))
                        {
                            let group = sub_entry.by_speed(speed).by_mode(mode);
                            stats += group.stats.to_owned();

                            for (idx, game) in group.games.iter().copied() {
                                if latest_game.map_or(true, |(latest_idx, _game)| latest_idx < idx)
                                {
                                    latest_game = Some((idx, game));
                                }
                            }

                            recent_games.extend(
                                group
                                    .games
                                    .iter()
                                    .copied()
                                    .map(|(idx, game)| (idx, uci.to_owned(), game)),
                            );
                        }
                    }
                }
            }

            if !stats.is_empty() || latest_game.is_some() {
                moves.push(PreparedMove {
                    uci,
                    stats: stats.clone(),
                    average_rating: None,
                    average_opponent_rating: stats.average_rating(),
                    game: latest_game.filter(|_| stats.is_single()).map(|(_, id)| id),
                });

                total += stats;
            }
        }

        moves.sort_by_key(|row| Reverse(row.stats.total()));
        recent_games.sort_by_key(|(idx, _, _)| Reverse(*idx));

        PreparedResponse {
            total,
            moves,
            recent_games: recent_games
                .into_iter()
                .map(|(_, uci, game)| (uci, game))
                .take(MAX_PLAYER_GAMES as usize)
                .collect(),
            top_games: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub struct PlayerStatus {
    pub latest_created_at: u64,
    pub revisit_ongoing_created_at: Option<u64>,
    pub indexed_at: SystemTime,
    pub revisited_at: SystemTime,
}

impl Default for PlayerStatus {
    fn default() -> PlayerStatus {
        PlayerStatus {
            latest_created_at: 0,
            revisit_ongoing_created_at: None,
            indexed_at: SystemTime::UNIX_EPOCH,
            revisited_at: SystemTime::UNIX_EPOCH,
        }
    }
}

impl PlayerStatus {
    pub const SIZE_HINT: usize = 3 * 8;

    pub fn maybe_revisit_ongoing(&mut self) -> Option<IndexRun> {
        if SystemTime::now()
            .duration_since(self.revisited_at)
            .unwrap_or_default()
            > Duration::from_secs(24 * 60 * 60)
        {
            self.revisit_ongoing_created_at
                .map(|since| IndexRun::Revisit { since })
        } else {
            None
        }
    }

    pub fn maybe_index(&self) -> Option<IndexRun> {
        SystemTime::now()
            .duration_since(self.indexed_at)
            .map_or(false, |cooldown| cooldown > Duration::from_secs(60))
            .then(|| IndexRun::Index {
                after: self.latest_created_at,
            })
    }

    pub fn finish_run(&mut self, run: IndexRun) {
        self.indexed_at = SystemTime::now();
        if matches!(run, IndexRun::Revisit { .. }) {
            self.revisited_at = self.indexed_at;
        }
    }

    pub fn read<R: Read>(reader: &mut R) -> io::Result<PlayerStatus> {
        Ok(PlayerStatus {
            latest_created_at: read_uint(reader)?,
            revisit_ongoing_created_at: Some(read_uint(reader)?).filter(|t| *t != 0),
            indexed_at: SystemTime::UNIX_EPOCH + Duration::from_secs(read_uint(reader)?),
            revisited_at: SystemTime::UNIX_EPOCH + Duration::from_secs(read_uint(reader)?),
        })
    }

    pub fn write<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        write_uint(writer, self.latest_created_at)?;
        write_uint(writer, self.revisit_ongoing_created_at.unwrap_or(0))?;
        write_uint(
            writer,
            self.indexed_at
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("duration since unix epoch")
                .as_secs(),
        )?;
        write_uint(
            writer,
            self.revisited_at
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("duration since unix epoch")
                .as_secs(),
        )
    }
}

#[derive(Debug)]
pub enum IndexRun {
    Index { after: u64 },
    Revisit { since: u64 },
}

impl IndexRun {
    pub fn since(&self) -> u64 {
        match *self {
            IndexRun::Index { after } => {
                // Plus 1 millisecond, as an optimization to avoid overlap.
                // Might miss games if the index run happens between games
                // created in the same millisecond.
                after.saturating_add(1)
            }
            IndexRun::Revisit { since } => since,
        }
    }
}

impl fmt::Display for IndexRun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IndexRun::Index { after } => write!(f, "created_at > {}", after),
            IndexRun::Revisit { since } => write!(f, "created_at >= {}", since),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use shakmaty::{Color, Square};

    use super::*;

    #[test]
    fn test_header_roundtrip() {
        let headers = [
            Header::Group {
                mode: Mode::Rated,
                speed: Speed::Correspondence,
                num_games: 15,
            },
            Header::End,
        ];

        let mut writer = Cursor::new(Vec::new());
        for header in &headers {
            header.write(&mut writer).unwrap();
        }

        let mut reader = Cursor::new(writer.into_inner());
        for header in headers {
            assert_eq!(Header::read(&mut reader).unwrap(), header);
        }
    }

    #[test]
    fn test_merge_player() {
        // Merge three entries, two of which are for the same move in the same
        // group.
        let uci_ab = Uci::Normal {
            from: Square::E2,
            to: Square::E4,
            promotion: None,
        };

        let a = PlayerEntry::new_single(
            uci_ab.clone(),
            Speed::Bullet,
            Mode::Rated,
            "aaaaaaaa".parse().unwrap(),
            Outcome::Decisive {
                winner: Color::White,
            },
            1600,
        );

        let b = PlayerEntry::new_single(
            uci_ab.clone(),
            Speed::Bullet,
            Mode::Rated,
            "bbbbbbbb".parse().unwrap(),
            Outcome::Decisive {
                winner: Color::Black,
            },
            1800,
        );

        let uci_c = Uci::Normal {
            from: Square::D2,
            to: Square::D4,
            promotion: None,
        };

        let c = PlayerEntry::new_single(
            uci_c,
            Speed::Bullet,
            Mode::Rated,
            "cccccccc".parse().unwrap(),
            Outcome::Draw,
            1700,
        );

        let mut cursor = Cursor::new(Vec::new());
        a.write(&mut cursor).unwrap();
        assert_eq!(
            cursor.position() as usize,
            PlayerEntry::SIZE_HINT,
            "optimized for single entries"
        );

        let mut deserialized = PlayerEntry::default();
        deserialized
            .extend_from_reader(&mut Cursor::new(cursor.into_inner()))
            .unwrap();

        let mut cursor = Cursor::new(Vec::new());
        b.write(&mut cursor).unwrap();
        deserialized
            .extend_from_reader(&mut Cursor::new(cursor.into_inner()))
            .unwrap();

        let mut cursor = Cursor::new(Vec::new());
        c.write(&mut cursor).unwrap();
        deserialized
            .extend_from_reader(&mut Cursor::new(cursor.into_inner()))
            .unwrap();

        assert_eq!(deserialized.sub_entries.len(), 2);
        assert_eq!(deserialized.max_game_idx, Some(2));
        let group = deserialized
            .sub_entries
            .get(&uci_ab)
            .unwrap()
            .by_speed(Speed::Bullet)
            .by_mode(Mode::Rated);
        assert_eq!(group.stats.white, 1);
        assert_eq!(group.stats.draws, 0);
        assert_eq!(group.stats.black, 1);
        assert_eq!(group.stats.average_rating(), Some(1700));
        assert_eq!(group.games.len(), 2);

        // Roundtrip the combined entry.
        let mut cursor = Cursor::new(Vec::new());
        deserialized.write(&mut cursor).unwrap();
        let mut deserialized = PlayerEntry::default();
        deserialized
            .extend_from_reader(&mut Cursor::new(cursor.into_inner()))
            .unwrap();
        assert_eq!(deserialized.sub_entries.len(), 2);
        assert_eq!(deserialized.max_game_idx, Some(2));
    }
}