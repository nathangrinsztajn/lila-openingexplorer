use super::{read_uint, write_uint, ByMode, BySpeed, GameId, Mode, Record, Speed, ByUci};
use byteorder::{ReadBytesExt as _, WriteBytesExt as _};
use std::cmp::min;
use std::io::{self, Read, Write};
use std::ops::AddAssign;
use std::cmp::max;

const MAX_GAMES: usize = 15; // 4 bits

#[derive(Debug, Eq, PartialEq)]
enum Header {
    Group {
        mode: Mode,
        speed: Speed,
        num_games: usize,
    },
    End,
}

impl Record for Header {
    fn read<R: Read>(reader: &mut R) -> io::Result<Header> {
        let n = reader.read_u8()?;
        Ok(Header::Group {
            speed: match n & 7 {
                0 => return Ok(Header::End),
                1 => Speed::Ultrabullet,
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
                    Speed::Ultrabullet => 1,
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

#[derive(Debug, Default)]
struct Stats {
    white: u64,
    draw: u64,
    black: u64,
}

impl AddAssign for Stats {
    fn add_assign(&mut self, rhs: Stats) {
        self.white += rhs.white;
        self.draw += rhs.draw;
        self.black += rhs.black;
    }
}

impl Record for Stats {
    fn read<R: Read>(reader: &mut R) -> io::Result<Stats> {
        Ok(Stats {
            white: read_uint(reader)?,
            draw: read_uint(reader)?,
            black: read_uint(reader)?,
        })
    }

    fn write<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        write_uint(writer, self.white)?;
        write_uint(writer, self.draw)?;
        write_uint(writer, self.black)
    }
}

#[derive(Default)]
struct Group {
    stats: Stats,
    games: Vec<(usize, GameId)>,
}

#[derive(Default)]
struct SubEntry {
    inner: BySpeed<ByMode<Group>>,
    max_game_idx: usize,
}

impl Record for SubEntry {
    fn read<R: Read>(reader: &mut R) -> io::Result<SubEntry> {
        let mut acc = SubEntry::default();
        loop {
            match Header::read(reader)? {
                Header::Group {
                    speed,
                    mode,
                    num_games,
                } => {
                    let stats = Stats::read(reader)?;
                    let mut games = Vec::with_capacity(num_games);
                    for _ in 0..num_games {
                        let game_idx = usize::from(reader.read_u8()?);
                        acc.max_game_idx = max(acc.max_game_idx, game_idx);
                        let game = GameId::read(reader)?;
                        games.push((game_idx, game));
                    }
                    let group = acc.inner.by_speed_mut(speed).by_mode_mut(mode);
                    *group = Group { stats, games };
                }
                Header::End => break,
            }
        }
        Ok(acc)
    }

    fn write<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        self.inner.as_ref().try_map(|speed, by_mode| {
            by_mode.as_ref().try_map(|mode, group| {
                let num_games = min(group.games.len(), MAX_GAMES);

                Header::Group {
                    speed,
                    mode,
                    num_games,
                }
                .write(writer)?;

                group.stats.write(writer)?;

                for (game_idx, game) in group.games.iter().take(num_games) {
                    writer.write_u8(*game_idx as u8)?;
                    game.write(writer)?;
                }

                Ok::<_, io::Error>(())
            })
        })?;

        Header::End.write(writer)
    }
}

struct Entry {
    inner: ByUci<SubEntry>,
}

impl Entry {
    fn max_game_idx(&self) -> usize {
        self.inner.0.values().map(|v| v.max_game_idx).max().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

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
}
