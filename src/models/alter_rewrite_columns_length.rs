use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() == sea_orm::DatabaseBackend::Sqlite {
            return Ok(());
        }

        if manager
            .has_column("rustpbx_call_records", "rewrite_original_from")
            .await?
        {
            manager
                .alter_table(
                    Table::alter()
                        .table(crate::models::call_record::Entity)
                        .modify_column(
                            ColumnDef::new(crate::models::call_record::Column::RewriteOriginalFrom)
                                .string()
                                .null(),
                        )
                        .to_owned(),
                )
                .await
                .ok();
        }

        if manager
            .has_column("rustpbx_call_records", "rewrite_original_to")
            .await?
        {
            manager
                .alter_table(
                    Table::alter()
                        .table(crate::models::call_record::Entity)
                        .modify_column(
                            ColumnDef::new(crate::models::call_record::Column::RewriteOriginalTo)
                                .string()
                                .null(),
                        )
                        .to_owned(),
                )
                .await
                .ok();
        }
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
