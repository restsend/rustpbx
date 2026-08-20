use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let table_name = "rustpbx_call_records";

        if !manager.has_column(table_name, "session_id").await? {
            manager
                .alter_table(
                    Table::alter()
                        .table(super::call_record::Entity)
                        .add_column(
                            ColumnDef::new(super::call_record::Column::SessionId)
                                .string_len(120)
                                .null(),
                        )
                        .to_owned(),
                )
                .await?;
        }

        if !manager
            .has_index(table_name, "idx_rustpbx_call_records_session_id")
            .await?
        {
            manager
                .create_index(
                    Index::create()
                        .name("idx_rustpbx_call_records_session_id")
                        .table(super::call_record::Entity)
                        .col(super::call_record::Column::SessionId)
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
