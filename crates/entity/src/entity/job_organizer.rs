//! `SeaORM` Entity. Generated by sea-orm-codegen 0.12.15

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize, Default)]
#[sea_orm(table_name = "job_organizer")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: u64,
    pub eid: String,
    pub name: String,
    pub nodes: Option<Json>,
    pub edges: Option<Json>,
    pub info: String,
    pub is_public: i8,
    pub created_user: String,
    pub updated_user: String,
    pub created_time: DateTimeLocal,
    pub updated_time: DateTimeLocal,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
