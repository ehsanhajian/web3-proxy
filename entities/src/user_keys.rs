//! SeaORM Entity. Generated by sea-orm-codegen 0.9.1

use sea_orm::entity::prelude::*;
use sea_orm::prelude::Uuid;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "user_keys")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: u64,
    pub user_id: u64,
    #[sea_orm(unique)]
    pub api_key: Uuid,
    pub description: Option<String>,
    pub private_txs: bool,
    pub active: bool,
    pub requests_per_minute: Option<u64>,
    #[sea_orm(column_type = "Decimal(Some((5, 4)))")]
    pub log_revert_chance: Decimal,
    #[sea_orm(column_type = "Text", nullable)]
    pub allowed_ips: Option<String>,
    #[sea_orm(column_type = "Text", nullable)]
    pub allowed_origins: Option<String>,
    #[sea_orm(column_type = "Text", nullable)]
    pub allowed_referers: Option<String>,
    #[sea_orm(column_type = "Text", nullable)]
    pub allowed_user_agents: Option<String>,
    pub max_concurrent_requests: Option<u64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::user::Entity",
        from = "Column::UserId",
        to = "super::user::Column::Id",
        on_update = "NoAction",
        on_delete = "NoAction"
    )]
    User,
    #[sea_orm(has_many = "super::revert_logs::Entity")]
    RevertLogs,
    #[sea_orm(has_many = "super::rpc_accounting::Entity")]
    RpcAccounting,
}

impl Related<super::user::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::User.def()
    }
}

impl Related<super::revert_logs::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::RevertLogs.def()
    }
}

impl Related<super::rpc_accounting::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::RpcAccounting.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
