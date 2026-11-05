//! B271: the startup check names exactly the accounts whose password is "".

use pharos_core::{SecretString, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::{auth::BuiltinAuth, state::Stores};

async fn auth_with(users: &[(&str, &str)]) -> BuiltinAuth<Stores> {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores);
    // Straight into the store: `create_user` refuses "" now (V211), and
    // these stand for accounts stored before it did.
    for (name, password) in users {
        let record = UserRecord {
            id: UserId::new(),
            name: (*name).to_owned(),
            password_hash: auth.hash_password(&SecretString::new(*password)).unwrap(),
            policy: UserPolicy::default(),
        };
        auth.users().create(record).await.unwrap();
    }
    auth
}

async fn empty_password_names(auth: &BuiltinAuth<Stores>) -> Vec<String> {
    let records = auth.users().list().await.unwrap();
    let mut names = auth.names_with_empty_password(&records);
    names.sort();
    names
}

#[actix_web::test]
async fn the_report_names_exactly_the_accounts_with_an_empty_password() {
    let auth = auth_with(&[
        ("alice", ""),
        ("bob", "hunter2"),
        ("carol", ""),
        ("dave", "x"),
    ])
    .await;
    assert_eq!(empty_password_names(&auth).await, vec!["alice", "carol"]);
}

#[actix_web::test]
async fn no_empty_password_gives_an_empty_report() {
    let auth = auth_with(&[("bob", "hunter2")]).await;
    assert!(empty_password_names(&auth).await.is_empty());
}

#[actix_web::test]
async fn a_user_table_with_no_users_gives_an_empty_report() {
    let auth = auth_with(&[]).await;
    assert!(empty_password_names(&auth).await.is_empty());
}
