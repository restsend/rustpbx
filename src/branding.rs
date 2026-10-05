use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const DEFAULT_SITE_NAME: &str = "RustPBX";
pub const DEFAULT_PRIMARY_COLOR: &str = "#0284c7";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrandContext {
    pub site_name: String,
    pub logo_url: String,
    pub logo_mini_url: String,
    pub favicon_url: String,
    pub primary_color: String,
    pub footer_text: String,
    pub login_background_url: String,
    pub custom_css: String,
}

impl Default for BrandContext {
    fn default() -> Self {
        Self {
            site_name: DEFAULT_SITE_NAME.to_string(),
            logo_url: String::new(),
            logo_mini_url: String::new(),
            favicon_url: String::new(),
            primary_color: DEFAULT_PRIMARY_COLOR.to_string(),
            footer_text: String::new(),
            login_background_url: String::new(),
            custom_css: String::new(),
        }
    }
}

pub trait BrandingProvider: Send + Sync {
    fn brand(&self) -> BrandContext;

    fn brand_for_host(&self, _host: &str) -> Option<BrandContext> {
        None
    }

    fn email_from_name(&self) -> Option<String> {
        None
    }

    fn email_footer(&self) -> Option<String> {
        None
    }

    fn email_template(&self) -> Option<crate::mail::EmailTemplate> {
        None
    }
}

pub struct DefaultBranding;

impl BrandingProvider for DefaultBranding {
    fn brand(&self) -> BrandContext {
        BrandContext::default()
    }
}

pub fn resolve(provider: Option<&Arc<dyn BrandingProvider>>) -> BrandContext {
    provider.map(|p| p.brand()).unwrap_or_default()
}

pub fn resolve_for_host(
    provider: Option<&Arc<dyn BrandingProvider>>,
    host: Option<&str>,
) -> BrandContext {
    if let (Some(provider), Some(host)) = (provider, host) {
        if let Some(brand) = provider.brand_for_host(host) {
            return brand;
        }
    }
    resolve(provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestBrand;

    impl BrandingProvider for TestBrand {
        fn brand(&self) -> BrandContext {
            BrandContext {
                site_name: "Acme PBX".to_string(),
                primary_color: "#ff0000".to_string(),
                ..Default::default()
            }
        }
    }

    struct HostBrand;

    impl BrandingProvider for HostBrand {
        fn brand(&self) -> BrandContext {
            BrandContext {
                site_name: "Global".to_string(),
                ..Default::default()
            }
        }

        fn brand_for_host(&self, host: &str) -> Option<BrandContext> {
            if host == "pbx.example.com" {
                Some(BrandContext {
                    site_name: "Tenant".to_string(),
                    ..Default::default()
                })
            } else {
                None
            }
        }
    }

    #[test]
    fn default_context_is_the_community_brand() {
        let brand = BrandContext::default();
        assert_eq!(brand.site_name, DEFAULT_SITE_NAME);
        assert!(brand.logo_url.is_empty());
        assert!(!brand.primary_color.is_empty());
    }

    #[test]
    fn provider_overrides_and_none_falls_back() {
        let provider: Arc<dyn BrandingProvider> = Arc::new(TestBrand);
        let overridden = resolve(Some(&provider));
        assert_eq!(overridden.site_name, "Acme PBX");
        assert_eq!(overridden.primary_color, "#ff0000");
        assert_eq!(overridden.logo_url, BrandContext::default().logo_url);

        let fallback = resolve(None);
        assert_eq!(fallback.site_name, DEFAULT_SITE_NAME);
    }

    #[test]
    fn host_resolution_prefers_matching_host_then_falls_back() {
        let provider: Arc<dyn BrandingProvider> = Arc::new(HostBrand);
        assert_eq!(
            resolve_for_host(Some(&provider), Some("pbx.example.com")).site_name,
            "Tenant"
        );
        assert_eq!(
            resolve_for_host(Some(&provider), Some("other.example.com")).site_name,
            "Global"
        );
        assert_eq!(resolve_for_host(Some(&provider), None).site_name, "Global");
        assert_eq!(
            resolve_for_host(None, Some("pbx.example.com")).site_name,
            DEFAULT_SITE_NAME
        );
    }
}
