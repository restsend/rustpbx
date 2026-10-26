use sea_orm::{ConnectionTrait, DbBackend};
use sea_orm_migration::prelude::*;

/// Voicemail is opt-in: new extensions default to `voicemail_disabled = true`.
///
/// Only the column default changes; existing rows keep their stored value.
/// SQLite cannot change a column default in place, and every insert path in
/// the app sets the column explicitly, so SQLite keeps the old default.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        set_default(manager, true).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        set_default(manager, false).await
    }
}

async fn set_default(manager: &SchemaManager<'_>, disabled: bool) -> Result<(), DbErr> {
    if !manager
        .has_column("rustpbx_extensions", "voicemail_disabled")
        .await?
    {
        return Ok(());
    }
    let db = manager.get_connection();
    let value = match db.get_database_backend() {
        DbBackend::Postgres => if disabled { "TRUE" } else { "FALSE" },
        DbBackend::MySql => if disabled { "1" } else { "0" },
        _ => return Ok(()),
    };
    db.execute_unprepared(&format!(
        "ALTER TABLE rustpbx_extensions ALTER COLUMN voicemail_disabled SET DEFAULT {value}"
    ))
    .await?;
    Ok(())
}
