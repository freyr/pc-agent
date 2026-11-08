/// The minisign public key (base64, the second line of the `.pub` file) of
/// the key that signs `manifest.json` of each release. `release.yml` signs
/// with the secret key in the repo secret `MINISIGN_SECRET_KEY`.
// Key ID 519AA905763D27E3.
#[allow(dead_code)] // U2 checks the downloaded manifest with it.
pub const PUBLIC_KEY: &str = "RWTjJz12BamaURj6irD4G/8dHcR5ANhv3IDj0Rmi81ZD4NMX8BNeSpNC";
