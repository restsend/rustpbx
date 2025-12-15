use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let table = crate::models::call_record::Entity;
        let col_started_at = crate::models::call_record::Column::StartedAt;
        let col_trunk = crate::models::call_record::Column::SipTrunkId;
        let col_id = crate::models::call_record::Column::Id;
        let col_currency = crate::models::call_record::Column::BillingCurrency;
        let col_status = crate::models::call_record::Column::Status;
        let col_billing_status = crate::models::call_record::Column::BillingStatus;
        let col_billing_amount_total = crate::models::call_record::Column::BillingAmountTotal;
        let col_tags = crate::models::call_record::Column::Tags;
        let col_to_number = crate::models::call_record::Column::ToNumber;

        // Composite index on sip_trunk_id + started_at
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_rustpbx_call_records_trunk_started")
                    .table(table)
                    .col(col_trunk)
                    .col(col_started_at)
                    .to_owned(),
            )
            .await?;

        // Composite index on started_at + id + status (for pagination and stats)
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_rustpbx_call_records_started_id_status")
                    .table(table)
                    .col(col_started_at)
                    .col(col_id)
                    .col(col_status)
                    .to_owned(),
            )
            .await?;

        // Index on billing_currency
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_rustpbx_call_records_billing_currency")
                    .table(table)
                    .col(col_currency)
                    .to_owned(),
            )
            .await?;

        // Index on billing_status
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_rustpbx_call_records_billing_status")
                    .table(table)
                    .col(col_billing_status)
                    .to_owned(),
            )
            .await?;

        // Index on tags
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_rustpbx_call_records_tags")
                    .table(table)
                    .col(col_tags)
                    .to_owned(),
            )
            .await?;

        // Composite index on billing_currency + billing_amount_total
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_rustpbx_call_records_billing_currency_amount")
                    .table(table)
                    .col(col_currency)
                    .col(col_billing_amount_total)
                    .to_owned(),
            )
            .await?;

        // Index on to_number
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_rustpbx_call_records_to_number")
                    .table(table)
                    .col(col_to_number)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let table = crate::models::call_record::Entity;

        manager
            .drop_index(
                Index::drop()
                    .name("idx_rustpbx_call_records_to_number")
                    .table(table)
                    .to_owned(),
            )
            .await?;

        manager
            .drop_index(
                Index::drop()
                    .name("idx_rustpbx_call_records_billing_currency_amount")
                    .table(table)
                    .to_owned(),
            )
            .await?;

        manager
            .drop_index(
                Index::drop()
                    .name("idx_rustpbx_call_records_tags")
                    .table(table)
                    .to_owned(),
            )
            .await?;

        manager
            .drop_index(
                Index::drop()
                    .name("idx_rustpbx_call_records_billing_status")
                    .table(table)
                    .to_owned(),
            )
            .await?;

        manager
            .drop_index(
                Index::drop()
                    .name("idx_rustpbx_call_records_billing_currency")
                    .table(table)
                    .to_owned(),
            )
            .await?;

        manager
            .drop_index(
                Index::drop()
                    .name("idx_rustpbx_call_records_started_id_status")
                    .table(table)
                    .to_owned(),
            )
            .await?;

        manager
            .drop_index(
                Index::drop()
                    .name("idx_rustpbx_call_records_trunk_started")
                    .table(table)
                    .to_owned(),
            )
            .await
    }
}
