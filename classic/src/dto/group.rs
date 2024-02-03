/// A wrapper type for all requests/responses from this module.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct GroupBody<T> {
    pub group: T,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Group {
    pub id: uuid::Uuid,
    pub name: String,
}

#[derive(serde::Deserialize)]
pub struct NewGroup {
    pub name: String,
}

#[derive(serde::Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct UpdateGroup {
    pub name: Option<String>,
}
