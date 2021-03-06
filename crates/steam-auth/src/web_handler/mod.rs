use std::{cell::RefCell, rc::Rc};

use cookie::{Cookie, CookieJar};
use reqwest::Method;
use scraper::Html;
use tracing::{debug, info, instrument, trace};

use const_concat::const_concat;
use steam_totp::Time;

use crate::{
    client::MobileClient,
    errors::{ApiKeyError, LoginError},
    page_scraper::{api_key_resolve_status, confirmation_details_single, confirmation_retrieve},
    types::{
        ApiKeyRegisterRequest, BooleanResponse, ConfirmationDetailsResponse,
        ConfirmationMultiAcceptRequest, ParentalUnlockRequest, ParentalUnlockResponse,
    },
    utils::{dump_cookie_from_header, dump_cookies_by_name},
    web_handler::confirmation::{Confirmation, ConfirmationMethod},
    User, STEAM_COMMUNITY_BASE, STEAM_COMMUNITY_HOST, STEAM_STORE_BASE, STEAM_STORE_HOST,
};

pub mod confirmation;
pub(crate) mod login;
mod trade;

/// used to refresh session
const MOBILE_AUTH_GETWGTOKEN: &str = const_concat!(
    crate::STEAM_API_BASE,
    "/IMobileAuthService/GetWGToken/v0001"
);

async fn session_refresh() {}

/// This should be performed immediately after login
pub(crate) async fn parental_unlock(client: &MobileClient, user: &User) -> Result<(), LoginError> {
    let parental_code = user.parental_code.clone().unwrap();

    {
        parental_unlock_by_service(
            Rc::clone(&client.cookie_store),
            client,
            &parental_code,
            STEAM_COMMUNITY_BASE,
            STEAM_COMMUNITY_HOST,
        )
        .await?;
    }

    {
        parental_unlock_by_service(
            Rc::clone(&client.cookie_store),
            client,
            &parental_code,
            STEAM_STORE_BASE,
            STEAM_STORE_HOST,
        )
        .await?;
    }
    Ok(())
}

/// Try to unlock account with parental controls (Family Sharing).
async fn parental_unlock_by_service(
    cookie_jar: Rc<RefCell<CookieJar>>,
    client: &MobileClient,
    parental_control_code: &str,
    url: &str,
    cookie_host: &str,
) -> Result<(), LoginError> {
    let unlock_url = format!("{}/parental/ajaxunlock", url);
    let session_id = dump_cookies_by_name(&cookie_jar.borrow(), cookie_host, "sessionid").unwrap();

    let request = ParentalUnlockRequest {
        pin: parental_control_code,
        sessionid: &session_id,
    };
    let response = client
        .request(unlock_url, Method::POST, None, Some(request))
        .await?;

    let parental_cookie_name = "steamparental";
    if let Some(cookie) = dump_cookie_from_header(&response, parental_cookie_name) {
        let mut cookie_jar = cookie_jar.borrow_mut();
        cookie_jar.add_original(
            Cookie::build(parental_cookie_name, cookie.clone())
                .domain(STEAM_STORE_HOST)
                .path("/")
                .finish(),
        );
        cookie_jar.add_original(
            Cookie::build(parental_cookie_name, cookie)
                .domain(STEAM_COMMUNITY_HOST)
                .path("/")
                .finish(),
        );
    }

    let response = response.json::<ParentalUnlockResponse>().await.unwrap();
    if response.eresult != 1 {
        let error = format!("EResult: {}", response.eresult);
        return Err(LoginError::ParentalUnlock(error));
    }

    Ok(())
}

/// Resolve caching of the user APIKey.
/// This is done after user logon for the first time in this session.
async fn cache_resolve(client: &MobileClient, user: &User) {
    api_key_retrieve(client).await.unwrap();
    // steamid_retrieve(client).await.unwrap()
}

/// Send confirmations to Steam Servers for accepting/denying.
pub(crate) async fn confirmations_send(
    client: &MobileClient,
    user: &User,
    steamid: u64,
    method: ConfirmationMethod,
    confirmations: Vec<Confirmation>,
) -> Result<(), reqwest::Error> {
    let url = format!("{}/mobileconf/multiajaxop", STEAM_COMMUNITY_BASE);
    let operation = method.value();

    let mut id_vec = vec![];
    let mut key_vec = vec![];
    for confirmation in confirmations.into_iter() {
        id_vec.push(("cid[]", confirmation.id));
        key_vec.push(("ck[]", confirmation.key));
    }

    let (time, confirmation_hash, device_id) = generate_confirmation_query_params(user).await;
    let request = ConfirmationMultiAcceptRequest {
        steamid: &steamid.to_string(),
        confirmation_hash,
        operation,
        device_id,
        time: &time.to_string(),
        confirmation_id: id_vec,
        confirmation_key: key_vec,
        ..Default::default()
    };

    let response = client
        .request_with_session_guard(url, Method::POST, None, Some(request))
        .await?
        .json::<BooleanResponse>()
        .await?;

    // FIXME: Error Catching
    // if response.success {
    //     Ok(())
    // }

    Ok(())
}

/// Retrieve all confirmations for user, opting between retrieving details or not.
pub(crate) async fn confirmations_retrieve_all(
    client: &MobileClient,
    user: &User,
    steamid: u64,
    require_details: bool,
) -> Result<Option<Vec<Confirmation>>, reqwest::Error> {
    let (time, confirmation_hash, device_id) = generate_confirmation_query_params(user).await;

    let confirmation_all_url = format!(
        "{}/mobileconf/conf?a={}&k={}&l=english&m=android&p={}&t={}&tag=conf",
        STEAM_COMMUNITY_BASE, steamid, confirmation_hash, device_id, time
    );
    trace!("Confirmation url: {}", confirmation_all_url);

    let html = client.get_html(confirmation_all_url).await.unwrap();
    let user_confirmations = confirmation_retrieve(html);

    // There is no need for now for additional details of the confirmation..
    if !require_details || user_confirmations.is_none() {
        return Ok(user_confirmations);
    }

    // FIXME: Is there a need to fetch additional details?

    let mut user_confirmations = user_confirmations.unwrap();
    let conf_details_fut = user_confirmations
        .iter()
        .map(|confirmation| {
            let details_url = format!(
                "{}/mobileconf/details/{}?a={}&k={}&l=english&m=android&p={}&t={}&tag=conf",
                STEAM_COMMUNITY_BASE, confirmation.id, steamid, confirmation_hash, device_id, time
            );
            client.request(details_url, Method::GET, None, None::<&str>)
        })
        .collect::<Vec<_>>();

    let joined_fut = futures::future::join_all(conf_details_fut).await;
    let mut details_vec = Vec::new();
    for response in joined_fut {
        let response_content = response
            .unwrap()
            .json::<ConfirmationDetailsResponse>()
            .await
            .unwrap();
        let html = Html::parse_document(&response_content.html);
        details_vec.push(confirmation_details_single(html));
    }

    for (confirmation, detail) in user_confirmations.iter_mut().zip(details_vec.into_iter()) {
        confirmation.details = Some(detail);
    }

    Ok(Some(user_confirmations))
}

async fn generate_confirmation_query_params(user: &User) -> (Time, String, &str) {
    let time = Time::with_offset().await.unwrap();
    let identity_secret = user
        .identity_secret()
        .expect("You need to have a linked ma file to recover confirmations");
    let confirmation_hash =
        steam_totp::generate_confirmation_key(identity_secret, time, Some("conf")).unwrap();
    let device_id = user.device_id().expect("You need a linked device id");
    (time, confirmation_hash, device_id)
}

/// Retrieve user SteamID.
async fn steamid_retrieve(client: &MobileClient) {}

/// Retrieve user Api Key.
async fn api_key_retrieve(client: &MobileClient) -> Result<String, ApiKeyError> {
    let api_key_url = format!("{}{}", STEAM_COMMUNITY_BASE, "/dev/apikey?l=english");
    let doc = client.get_html(api_key_url).await?;
    let api = match api_key_resolve_status(doc) {
        Ok(api) => api,
        Err(ApiKeyError::NotRegistered) => {
            // in this case we want to register it
            api_key_register(&client).await?
        }
        Err(e) => return Err(e),
    };
    Ok(api)
}

/// Request access to an API Key
/// The account should be validated before.
async fn api_key_register(client: &MobileClient) -> Result<String, ApiKeyError> {
    let api_register_url = format!("{}{}", STEAM_COMMUNITY_BASE, "/dev/registerkey");
    let register_request = ApiKeyRegisterRequest::default();

    let response = client
        .request_with_session_guard(api_register_url, Method::POST, None, Some(register_request))
        .await?;

    Ok("".to_string())
}

#[cfg(test)]
mod tests {
    use reqwest::Url;

    #[test]
    fn test_path_handling() {
        let lost_auth = Url::parse("steammobile://lostauth/login").unwrap();
        assert_eq!("steammobile", lost_auth.scheme());
        assert_eq!("lostauth", lost_auth.host_str().unwrap());
        assert_eq!("/login", lost_auth.path());
    }
}
