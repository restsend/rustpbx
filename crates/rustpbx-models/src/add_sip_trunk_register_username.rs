use sea_orm_migration::prelude::*;

/// Keep existing authentication identities while allowing a separate REGISTER AoR user.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if !manager.has_column("rustpbx_sip_trunks", "register_username").await? {
            manager.alter_table(
                Table::alter()
                    .table(super::sip_trunk::Entity)
                    .add_column(ColumnDef::new(super::sip_trunk::Column::RegisterUsername)
                        .string_len(160).null())
                    .to_owned(),
            ).await?;
        }
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
