use sea_orm_migration::prelude::*;

/// Adds the `session_epoch` column to the rustpbx_users table. The epoch is
/// embedded in console session tokens and bumped whenever the password
/// changes, so existing sessions are invalidated on a password reset.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let table_name = "rustpbx_users";

        if !manager.has_column(table_name, "session_epoch").await? {
            manager
                .alter_table(
                    Table::alter()
                        .table(super::user::Entity)
                        .add_column(
                            ColumnDef::new(super::user::Column::SessionEpoch)
                                .big_integer()
                                .not_null()
                                .default(0),
                        )
                        .to_owned(),
                )
                .await?;
        }

        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
