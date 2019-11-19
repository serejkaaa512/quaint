mod conversion;
mod error;

use crate::{
    ast::{Id, ParameterizedValue, Query},
    connector::{metrics, queryable::*, ResultSet, DBIO},
    error::Error,
    visitor::{self, Visitor},
};
use futures::future;
use rusqlite::NO_PARAMS;
use std::{collections::HashSet, convert::TryFrom, path::Path, sync::Mutex};

/// A connector interface for the SQLite database
pub struct Sqlite {
    pub(crate) client: Mutex<rusqlite::Connection>,
    /// This is not a `PathBuf` because we need to `ATTACH` the database to the path, and this can
    /// only be done with UTF-8 paths.
    pub(crate) file_path: String,
}


pub struct SqliteParams {
    pub connection_limit: u32,
    /// This is not a `PathBuf` because we need to `ATTACH` the database to the path, and this can
    /// only be done with UTF-8 paths.
    pub file_path: String,
    pub db_name: Option<String>,
}

type ConnectionParams = (Vec<(String, String)>, Vec<(String, String)>);

impl TryFrom<&str> for SqliteParams {
    type Error = Error;

    fn try_from(path: &str) -> crate::Result<Self> {
        let path = path.trim_start_matches("file:");
        let path_parts: Vec<&str> = path.split('?').collect();
        let path_str = path_parts[0];
        let path = Path::new(path_str);

        if path.is_dir() {
            Err(Error::DatabaseUrlIsInvalid(
                path.to_str().unwrap().to_string(),
            ))
        } else {
            let official = vec![];
            let mut connection_limit = num_cpus::get_physical() * 2 + 1;
            let mut db_name = None;

            if path_parts.len() > 1 {
                let (_, unsupported): ConnectionParams = path_parts
                    .last()
                    .unwrap()
                    .split('&')
                    .map(|kv| {
                        let splitted: Vec<&str> = kv.split('=').collect();
                        (String::from(splitted[0]), String::from(splitted[1]))
                    })
                    .collect::<Vec<(String, String)>>()
                    .into_iter()
                    .partition(|(k, _)| official.contains(&k.as_str()));

                for (k, v) in unsupported.into_iter() {
                    match k.as_ref() {
                        "connection_limit" => {
                            let as_int: usize =
                                v.parse().map_err(|_| Error::InvalidConnectionArguments)?;

                            connection_limit = as_int;
                        }
                        "db_name" => {
                            db_name = Some(v.to_string());
                        }
                        _ => {
                            #[cfg(not(feature = "tracing-log"))]
                            trace!("Discarding connection string param: {}", k);
                            #[cfg(feature = "tracing-log")]
                            tracing::trace!(
                                message = "Discarding connection string param",
                                param = k.as_str()
                            );
                        }
                    };
                }
            }

            Ok(Self {
                connection_limit: u32::try_from(connection_limit).unwrap(),
                file_path: path_str.to_owned(),
                db_name,
            })
        }
    }
}

impl TryFrom<&str> for Sqlite {
    type Error = Error;

    fn try_from(path: &str) -> crate::Result<Self> {
        let params = SqliteParams::try_from(path)?;
        let client = Mutex::new(rusqlite::Connection::open_in_memory()?);
        let file_path = params.file_path;

        Ok(Sqlite { client, file_path })
    }
}

impl Sqlite {
    pub fn new(file_path: &str) -> crate::Result<Sqlite>
    {
        Self::try_from(file_path)
    }

    pub fn attach_database(&mut self, db_name: &str) -> crate::Result<()> {
        let client = self.client.lock().unwrap();
        let mut stmt = client.prepare("PRAGMA database_list")?;

        let databases: HashSet<String> = stmt
            .query_map(NO_PARAMS, |row| {
                let name: String = row.get(1)?;

                Ok(name)
            })?
            .map(|res| res.unwrap())
            .collect();

        if !databases.contains(db_name) {
            rusqlite::Connection::execute(
                &client,
                "ATTACH DATABASE ? AS ?",
                &[self.file_path.as_str(), db_name],
            )?;
        }

        rusqlite::Connection::execute(&client, "PRAGMA foreign_keys = ON", NO_PARAMS)?;

        Ok(())
    }
}

impl TransactionCapable for Sqlite {}

impl Queryable for Sqlite {
    fn execute<'a>(&'a self, q: Query<'a>) -> DBIO<'a, Option<Id>> {
        DBIO::new(async move {
            let (sql, params) = visitor::Sqlite::build(q);

            self.execute_raw(&sql, &params).await?;

            let client = self.client.lock().unwrap();
            let res = Some(Id::Int(client.last_insert_rowid() as usize));

            Ok(res)
        })
    }

    fn query<'a>(&'a self, q: Query<'a>) -> DBIO<'a, ResultSet> {
        let (sql, params) = visitor::Sqlite::build(q);

        DBIO::new(async move { self.query_raw(&sql, &params).await })
    }

    fn query_raw<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [ParameterizedValue],
    ) -> DBIO<'a, ResultSet> {
        metrics::query("sqlite.query_raw", sql, params, move || {
            let res = move || {
                let client = self.client.lock().unwrap();
                let mut stmt = client.prepare_cached(sql)?;
                let mut rows = stmt.query(params)?;

                let mut result = ResultSet::new(rows.to_column_names(), Vec::new());

                while let Some(row) = rows.next()? {
                    result.rows.push(row.to_result_row()?);
                }

                Ok(result)
            };

            match res() {
                Ok(res) => future::ok(res),
                Err(e) => future::err(e),
            }
        })
    }

    fn execute_raw<'a>(&'a self, sql: &'a str, params: &'a [ParameterizedValue]) -> DBIO<'a, u64> {
        metrics::query("sqlite.execute_raw", sql, params, move || {
            let res = move || {
                let client = self.client.lock().unwrap();

                let mut stmt = client.prepare_cached(sql)?;
                let changes = stmt.execute(params)?;

                Ok(u64::try_from(changes).unwrap())
            };

            match res() {
                Ok(res) => future::ok(res),
                Err(e) => future::err(e),
            }
        })
    }

    fn turn_off_fk_constraints(&self) -> DBIO<()> {
        DBIO::new(async move {
            self.query_raw("PRAGMA foreign_keys = OFF", &[]).await?;
            Ok(())
        })
    }

    fn turn_on_fk_constraints(&self) -> DBIO<()> {
        DBIO::new(async move {
            self.query_raw("PRAGMA foreign_keys = ON", &[]).await?;
            Ok(())
        })
    }

    fn raw_cmd<'a>(&'a self, cmd: &'a str) -> DBIO<'a, ()> {
        metrics::query("sqlite.raw_cmd", cmd, &[], move || {
            let client = self.client.lock().unwrap();

            match client.execute_batch(cmd) {
                Ok(_) => future::ok(()),
                Err(e) => future::err(e.into()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::{Queryable, TransactionCapable};

    #[test]
    fn sqlite_params_from_str_should_resolve_path_correctly() {
        let path = "file:dev.db";
        let params = SqliteParams::try_from(path).unwrap();
        assert_eq!(params.file_path, "dev.db");
    }

    #[tokio::test]
    async fn should_provide_a_database_connection() {
        let connection = Sqlite::new("db/test.db").unwrap();
        let res = connection
            .query_raw("SELECT * FROM sqlite_master", &[])
            .await
            .unwrap();

        assert!(res.is_empty());
    }

    #[tokio::test]
    async fn should_provide_a_database_transaction() {
        let connection = Sqlite::new("db/test.db").unwrap();
        let tx = connection.start_transaction().await.unwrap();
        let res = tx
            .query_raw("SELECT * FROM sqlite_master", &[])
            .await
            .unwrap();

        assert!(res.is_empty());
    }

    #[allow(unused)]
    const TABLE_DEF: &str = r#"
    CREATE TABLE USER (
        ID INT PRIMARY KEY     NOT NULL,
        NAME           TEXT    NOT NULL,
        AGE            INT     NOT NULL,
        SALARY         REAL
    );
    "#;

    #[allow(unused)]
    const CREATE_USER: &str = r#"
    INSERT INTO USER (ID,NAME,AGE,SALARY)
    VALUES (1, 'Joe', 27, 20000.00 );
    "#;

    #[tokio::test]
    async fn should_map_columns_correctly() {
        let connection = Sqlite::try_from("file:db/test.db").unwrap();

        connection.query_raw(TABLE_DEF, &[]).await.unwrap();
        connection.query_raw(CREATE_USER, &[]).await.unwrap();

        let rows = connection
            .query_raw("SELECT * FROM USER", &[])
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);

        let row = rows.get(0).unwrap();
        assert_eq!(row["ID"].as_i64(), Some(1));
        assert_eq!(row["NAME"].as_str(), Some("Joe"));
        assert_eq!(row["AGE"].as_i64(), Some(27));
        assert_eq!(row["SALARY"].as_f64(), Some(20000.0));
    }
}
