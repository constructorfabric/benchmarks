extern crate toolkit_canonical_errors;

use toolkit_canonical_errors::CanonicalError;

fn main() {
    // with_retry_after_seconds must only be available on ServiceUnavailableBuilder
    let _err = CanonicalError::internal("bug")
        .with_retry_after_seconds(5)
        .create();
}
