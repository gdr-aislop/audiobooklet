// `sqlx::migrate!` embeds the migrations directory at compile time; without this, adding a
// migration file doesn't rebuild the crate and the new migration is silently left out.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
