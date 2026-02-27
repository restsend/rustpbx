use sea_orm_migration::prelude::*;

/// Adds mfa_enabled, mfa_secret, and auth_source columns to the rustpbx_users
/// table for databases that were created before these fields were introduced.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let table_name = "rustpbx_users";

        if !manager.has_column(table_name, "mfa_enabled").await? {
            manager
                .alter_table(
                    Table::alter()
                        .table(super::user::Entity)
                        .add_column(
                            ColumnDef::new(super::user::Column::MfaEnabled)
                                .boolean()
                                .not_null()
                                .default(false),
                        )
                        .to_owned(),
                )
                .await?;
        }

        if !manager.has_column(table_name, "mfa_secret").await? {
            manager
                .alter_table(
                    Table::alter()
                        .table(super::user::Entity)
                        .add_column(
                            ColumnDef::new(super::user::Column::MfaSecret)
                                .string()
                                .char_len(64)
                                .null(),
                        )
                        .to_owned(),
                )
                .await?;
        }

        if !manager.has_column(table_name, "auth_source").await? {
            manager
                .alter_table(
                    Table::alter()
                        .table(super::user::Entity)
                        .add_column(
                            ColumnDef::new(super::user::Column::AuthSource)
                                .string()
                                .char_len(32)
                                .not_null()
                                .default("local"),
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
