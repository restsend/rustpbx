use sea_orm_migration::prelude::*;

/// Adds register_enabled, register_expires, and register_extra_headers columns
/// to the rustpbx_sip_trunks table for databases created before these fields
/// were introduced.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let table_name = "rustpbx_sip_trunks";

        if !manager.has_column(table_name, "register_enabled").await? {
            manager
                .alter_table(
                    Table::alter()
                        .table(super::sip_trunk::Entity)
                        .add_column(
                            ColumnDef::new(super::sip_trunk::Column::RegisterEnabled)
                                .boolean()
                                .not_null()
                                .default(false),
                        )
                        .to_owned(),
                )
                .await?;
        }

        if !manager.has_column(table_name, "register_expires").await? {
            manager
                .alter_table(
                    Table::alter()
                        .table(super::sip_trunk::Entity)
                        .add_column(
                            ColumnDef::new(super::sip_trunk::Column::RegisterExpires)
                                .integer()
                                .null(),
                        )
                        .to_owned(),
                )
                .await?;
        }

        if !manager
            .has_column(table_name, "register_extra_headers")
            .await?
        {
            manager
                .alter_table(
                    Table::alter()
                        .table(super::sip_trunk::Entity)
                        .add_column(
                            ColumnDef::new(super::sip_trunk::Column::RegisterExtraHeaders)
                                .json()
                                .null(),
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
