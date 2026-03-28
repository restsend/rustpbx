use sea_orm_migration::prelude::*;

/// Adds rewrite_hostport column to the rustpbx_sip_trunks table
/// for databases created before this field was introduced.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let table_name = "rustpbx_sip_trunks";

        if !manager.has_column(table_name, "rewrite_hostport").await? {
            manager
                .alter_table(
                    Table::alter()
                        .table(super::sip_trunk::Entity)
                        .add_column(
                            ColumnDef::new(super::sip_trunk::Column::RewriteHostport)
                                .boolean()
                                .not_null()
                                .default(true),
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
