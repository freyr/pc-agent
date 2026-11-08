/// The minisign public key (base64, the second line of the `.pub` file) of
/// the key that signs `manifest.json` of each release. `release.yml` signs
/// with the secret key in the repo secret `MINISIGN_SECRET_KEY`.
// TEMPORARY: replaced with the project key before merge
#[allow(dead_code)] // U2 checks the downloaded manifest with it.
pub const PUBLIC_KEY: &str = "RWS88peot50qA/8XeL+6z8NFzhCyn3GP5/r1jExSRYNgWtSRst6Pe6qK";
