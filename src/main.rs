use std::env;

use aws_config::BehaviorVersion;
use aws_sdk_cloudfront::types::{Origin, Origins};
use aws_sdk_cloudfront::Client as CloudFrontClient;
use lambda_http::{run, service_fn, Error, Request, Response, IntoResponse, http::{StatusCode, Method}};
use serde::{Deserialize, Serialize};
use lambda_http::RequestPayloadExt;
use aws_sdk_cloudfront::types::{
    AllowedMethods, CacheBehavior, CacheBehaviors, Method as CFMethod, ViewerProtocolPolicy,
};

// Tu estructura de entrada se mantiene igual
#[derive(Deserialize)]
#[derive(Debug)]
struct DeployRequest {
    distribution_id: String,
    oac_id: String, // El ID de tu CloudFrontOAC fijo
    host_bucket_name: String,
    recipes_bucket_name: String,
    alb_dns_name: String,
}

// Tu estructura de salida para respuestas exitosas
#[derive(Serialize)]
struct DeployResponse {
    message: String,
}

// Estructura auxiliar para devolver errores HTTP limpios en JSON
#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    // Inicializamos el runtime HTTP de Lambda
    let func = service_fn(handler);
    run(func).await?;
    Ok(())
}

async fn handler(event: Request) -> Result<impl IntoResponse, Error> {
    let path = event.uri().path();
    let method = event.method();

    // 1. Manejo del Preflight (OPTIONS) exigido por el navegador
    if method == Method::OPTIONS {
        return Ok(Response::builder()
            .status(StatusCode::OK)
            // Agregamos los headers mínimos para que el navegador valide el preflight
            .header("allow", "POST, OPTIONS")
            .header("access-control-allow-methods", "POST, OPTIONS")
            .header("access-control-allow-headers", "content-type")
            .body("".to_string())?);
    }

    // 2. Enrutamiento estricto para el negocio: Validamos que sea un POST a /api/create
    if method != Method::POST || path != "/api/promote" {
        return Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header("content-type", "application/json")
            .body(serde_json::to_string(&ErrorResponse {
                error: format!("Ruta no encontrada: {} {}", method, path),
            })?)?);
    }

    // 2. Extraer y deserializar el body JSON
    // payload_payload_json() es un helper de lambda_http para parsear el body directamente
    let payload = match event.payload::<DeployRequest>() {
        Ok(Some(p)) => p,
        err => {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header("content-type", "application/json")
                .body(serde_json::to_string(&err.unwrap_err().to_string())?)?);
        }
    };

    // 3. Inicializar AWS SDK (Mantenemos tu lógica original)
    let config = aws_config::load_defaults(BehaviorVersion::latest()).await;
    let cf_client = CloudFrontClient::new(&config);

    // 4. Ejecutar el despliegue
    match promote_environment(
        &cf_client,
        &payload.distribution_id,
        &payload.host_bucket_name,
        &payload.oac_id,
        &payload.recipes_bucket_name,
        &payload.alb_dns_name
    ).await {
        Ok(()) => {
            let res = DeployResponse {
                message: format!("Stack promovido"),
                
            };
            
            // Retornamos un 200 OK con el JSON correspondiente
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(serde_json::to_string(&res)?)?)
        }
        Err(err) => {
            // Si falla CloudFormation, respondemos con un 500 Internal Server Error
            Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header("content-type", "application/json")
                .body(serde_json::to_string(&ErrorResponse {
                    error: format!("Error al crear el stack de CloudFormation: {:?}", err),
                })?)?)
        }
    }
}

/// Función para ejecutar el create_stack (Se mantiene idéntica a tu código original)
async fn promote_environment(
    client: &CloudFrontClient,
    distribution_id: &str,
    host_bucket_name: &str,
    oac_id: &str,
    recipes_bucket_name: &str,
    alb_dns_name: &str
) -> Result<(), Error> {
    let get_config_output = client
        .get_distribution_config()
        .id(distribution_id)
        .send()
        .await?;

    // El ETag es obligatorio para el update posterior (control de concurrencia u optimistic locking)
    let etag = get_config_output.e_tag().ok_or("No se pudo obtener el ETag")?;
    
    // Obtenemos la configuración actual (es un Option, por eso el ok_or)
    let current_config = get_config_output.distribution_config()
        .ok_or("No se pudo obtener la configuración de la distribución")?;

    let empty_custom_headers = aws_sdk_cloudfront::types::CustomHeaders::builder()
    .quantity(0) // <-- Obligatorio indicar que la cantidad es 0
    .build()?;

    let region= env::var("AWS_REGION").expect("variable AWS_REGION no existe!");

    // --- 1. S3HostOrigin ---
    let s3_host_origin = Origin::builder()
        .id("S3HostOrigin")
        .domain_name(format!("{}.s3.{}.amazonaws.com", host_bucket_name, &region))
        .origin_path("")
        .origin_access_control_id(oac_id)
        .custom_headers(empty_custom_headers.clone())
        .s3_origin_config(
            aws_sdk_cloudfront::types::S3OriginConfig::builder()
                .origin_access_identity("")
                .build()
        )
        .build()?;

    // --- 2. S3RecipesOrigin ---
    let s3_recipes_origin = Origin::builder()
        .id("S3RecipesOrigin")
        .domain_name(format!("{}.s3.{}.amazonaws.com", recipes_bucket_name, &region))
        .origin_path("")
        .custom_headers(empty_custom_headers.clone())
        .origin_access_control_id(oac_id)
        .s3_origin_config(
            aws_sdk_cloudfront::types::S3OriginConfig::builder()
                .origin_access_identity("")
                .build()
        )
        .build()?;

    // Helper para configurar el protocolo TLS v1.2 obligatorio en los ALBs
    let ssl_protocols = aws_sdk_cloudfront::types::OriginSslProtocols::builder()
        .items(aws_sdk_cloudfront::types::SslProtocol::TlSv12)
        .quantity(1)
        .build()?;

    // --- 3. ALBOrigin3001 ---
    let alb_origin_3001 = Origin::builder()
        .id("ALBOrigin3001")
        .domain_name(alb_dns_name)
        .origin_path("")
        .custom_headers(empty_custom_headers.clone())
        .custom_origin_config(
            aws_sdk_cloudfront::types::CustomOriginConfig::builder()
                .http_port(3001)
                .https_port(443)
                .origin_read_timeout(30)      // <--- Solución: Timeout de lectura estándar (30s)
                .origin_keepalive_timeout(5) // <--- Recomendado: Keep-alive estándar (5s)
                .origin_protocol_policy(aws_sdk_cloudfront::types::OriginProtocolPolicy::HttpOnly)
                .origin_ssl_protocols(ssl_protocols.clone())
                .build()
                .unwrap()
        )
        .build()?;

    // --- 4. ALBOrigin3002 ---
    let alb_origin_3002 = Origin::builder()
        .id("ALBOrigin3002")
        .domain_name(alb_dns_name)
        .origin_path("")
        .custom_headers(empty_custom_headers.clone())
        .custom_origin_config(
            aws_sdk_cloudfront::types::CustomOriginConfig::builder()
                .http_port(3002)
                .https_port(443)
                .origin_read_timeout(30)      // <--- Solución: Timeout de lectura estándar (30s)
                .origin_keepalive_timeout(5) // <--- Recomendado: Keep-alive estándar (5s)
                .origin_protocol_policy(aws_sdk_cloudfront::types::OriginProtocolPolicy::HttpOnly)
                .origin_ssl_protocols(ssl_protocols)
                .build()
                .unwrap()
        )
        .build()?;

    // --- Agrupamos los 4 orígenes ---
    let new_origins = Origins::builder()
        .items(s3_host_origin)
        .items(s3_recipes_origin)
        .items(alb_origin_3001)
        .items(alb_origin_3002)
        .quantity(4)
        .build()?;

    // Modificamos el Default Cache Behavior para que apunte al nuevo Origin ID
    let mut new_default_behavior = current_config.default_cache_behavior()
        .ok_or("Falta el DefaultCacheBehavior")?
        .clone();
    new_default_behavior.target_origin_id = "S3HostOrigin".to_string();

    let s3_cached_methods = aws_sdk_cloudfront::types::CachedMethods::builder()
        .items(aws_sdk_cloudfront::types::Method::Get)
        .items(aws_sdk_cloudfront::types::Method::Head)
        .quantity(2)
        .build()?;

    // --- Configuración de Métodos para S3 (GET, HEAD, OPTIONS) ---
    let s3_allowed_methods = AllowedMethods::builder()
        .items(CFMethod::Get)
        .items(CFMethod::Head)
        .items(CFMethod::Options)
        .quantity(3)
        .cached_methods(s3_cached_methods)
        .build()?;

    let api_cached_methods = aws_sdk_cloudfront::types::CachedMethods::builder()
        .items(aws_sdk_cloudfront::types::Method::Get)
        .items(aws_sdk_cloudfront::types::Method::Head)
        .quantity(2)
        .build()?;

    // --- Configuración de Métodos para API/ALB (Todos los métodos HTTP) ---
    let api_allowed_methods = AllowedMethods::builder()
        .items(CFMethod::Get).items(CFMethod::Head).items(CFMethod::Options)
        .items(CFMethod::Put).items(CFMethod::Post).items(CFMethod::Patch).items(CFMethod::Delete)
        .quantity(7)
        .cached_methods(api_cached_methods)
        .build()?;

    // IDs de políticas que tenías en el YAML
    let caching_optimized_id = "658327ea-f89d-4fab-a63d-7e88639e58f6".to_string();
    let caching_disabled_id = "4135ea2d-6df8-44a3-9df3-4b5a84be39ad".to_string();
    let all_viewer_request_id = "216adef6-5c7f-47e4-b989-5492eafa07d3".to_string();

    let empty_lambda_associations = aws_sdk_cloudfront::types::LambdaFunctionAssociations::builder()
    .quantity(0) // 💡 Obligatorio indicar que la cantidad es 0
    .build()?;

    // 1. /apps/recipes (S3)
    let behavior_recipes_root = CacheBehavior::builder()
        .path_pattern("/apps/recipes")
        .target_origin_id("S3RecipesOrigin")
        .viewer_protocol_policy(ViewerProtocolPolicy::RedirectToHttps)
        .allowed_methods(s3_allowed_methods.clone())
        .compress(false)
        .smooth_streaming(false)
        .field_level_encryption_id("")
        .lambda_function_associations(empty_lambda_associations.clone())
        .cache_policy_id(&caching_optimized_id)
        .build()?;

    // 2. /apps/recipes/* (S3)
    let behavior_recipes_wildcard = CacheBehavior::builder()
        .path_pattern("/apps/recipes/*")
        .target_origin_id("S3RecipesOrigin")
        .viewer_protocol_policy(ViewerProtocolPolicy::RedirectToHttps)
        .allowed_methods(s3_allowed_methods)
        .compress(false)
        .smooth_streaming(false)
        .field_level_encryption_id("")
        .lambda_function_associations(empty_lambda_associations.clone())
        .cache_policy_id(&caching_optimized_id)
        .build()?;

    // 3. /api/recipes (ALB 3001)
    let behavior_api_recipes_root = CacheBehavior::builder()
        .path_pattern("/api/recipes")
        .target_origin_id("ALBOrigin3001")
        .viewer_protocol_policy(ViewerProtocolPolicy::RedirectToHttps)
        .allowed_methods(api_allowed_methods.clone())
        .cache_policy_id(&caching_disabled_id)
        .compress(false)
        .smooth_streaming(false)
        .field_level_encryption_id("")
        .lambda_function_associations(empty_lambda_associations.clone())
        .origin_request_policy_id(&all_viewer_request_id)
        .build()?;

    // 4. /api/recipes/* (ALB 3001)
    let behavior_api_recipes_wildcard = CacheBehavior::builder()
        .path_pattern("/api/recipes/*")
        .target_origin_id("ALBOrigin3001")
        .viewer_protocol_policy(ViewerProtocolPolicy::RedirectToHttps)
        .allowed_methods(api_allowed_methods.clone())
        .cache_policy_id(&caching_disabled_id)
        .compress(false)
        .smooth_streaming(false)
        .field_level_encryption_id("")
        .lambda_function_associations(empty_lambda_associations.clone())
        .origin_request_policy_id(&all_viewer_request_id)
        .build()?;

    // 5. /api/users (ALB 3002)
    let behavior_api_users_root = CacheBehavior::builder()
        .path_pattern("/api/users")
        .target_origin_id("ALBOrigin3002")
        .viewer_protocol_policy(ViewerProtocolPolicy::RedirectToHttps)
        .allowed_methods(api_allowed_methods.clone())
        .cache_policy_id(&caching_disabled_id)
        .compress(false)
        .smooth_streaming(false)
        .field_level_encryption_id("")
        .lambda_function_associations(empty_lambda_associations.clone())
        .origin_request_policy_id(&all_viewer_request_id)
        .build()?;

    // 6. /api/users/* (ALB 3002)
    let behavior_api_users_wildcard = CacheBehavior::builder()
        .path_pattern("/api/users/*")
        .target_origin_id("ALBOrigin3002")
        .viewer_protocol_policy(ViewerProtocolPolicy::RedirectToHttps)
        .allowed_methods(api_allowed_methods)
        .cache_policy_id(&caching_disabled_id)
        .smooth_streaming(false)
        .compress(false)
        .field_level_encryption_id("")
        .lambda_function_associations(empty_lambda_associations.clone())
        .origin_request_policy_id(&all_viewer_request_id)
        .build()?;

    // --- Agrupamos los 6 behaviors en la estructura contenedora ---
    let new_cache_behaviors = CacheBehaviors::builder()
        .items(behavior_recipes_root)
        .items(behavior_recipes_wildcard)
        .items(behavior_api_recipes_root)
        .items(behavior_api_recipes_wildcard)
        .items(behavior_api_users_root)
        .items(behavior_api_users_wildcard)
        .quantity(6) // <-- Clave: número exacto de ítems para evitar errores de validación
        .build()?;

    // Mapeamos explícitamente todos los campos obligatorios desde la configuración actual
    let updated_config = aws_sdk_cloudfront::types::DistributionConfig::builder()
        // 1. Pisamos con nuestros nuevos orígenes y behaviors efímeros
        .origins(new_origins)
        .default_cache_behavior(new_default_behavior)
        
        // 2. Conservamos el resto de la configuración crítica del stack base
        .caller_reference(current_config.caller_reference())
        .enabled(current_config.enabled())
        .comment(current_config.comment())
        
        // Aquí está la solución al error que tenías:
        .price_class(
            current_config.price_class()
                .cloned()
                .unwrap_or(aws_sdk_cloudfront::types::PriceClass::PriceClassAll)
        )
        
        // Conservamos los dominios (Aliases) y certificados
        .aliases(
            current_config.aliases()
                .cloned()
                .ok_or("Faltan los Aliases en la distribución actual")?
        )
        .viewer_certificate(
            current_config.viewer_certificate()
                .cloned()
                .ok_or("Falta el ViewerCertificate en la distribución actual")?
        )
        
        // Conservamos configuraciones opcionales/avanzadas (evita que se seteen a None)
        .restrictions(current_config.restrictions().cloned().unwrap())
        .web_acl_id(current_config.web_acl_id().unwrap_or_default())
        .http_version(
            current_config.http_version()
                .cloned()
                .unwrap_or(aws_sdk_cloudfront::types::HttpVersion::Http2)
        )
        .is_ipv6_enabled(current_config.is_ipv6_enabled().unwrap_or_default())
        .default_root_object(current_config.default_root_object().unwrap_or_default())
        .logging(current_config.logging().cloned().unwrap())
        .origin_groups(current_config.origin_groups().cloned().unwrap())
        .continuous_deployment_policy_id(current_config.continuous_deployment_policy_id().unwrap_or_default())
        .staging(current_config.staging().unwrap_or_default())
        
        // Si manejas los CacheBehaviors adicionales de las APIs por fuera, los pasas acá.
        // Si no los pasas, CloudFront asumirá que los querés borrar.
        .cache_behaviors(
            current_config.cache_behaviors().cloned().ok_or("Faltan los CacheBehaviors")?
        )
        .cache_behaviors(new_cache_behaviors)
        .custom_error_responses(current_config.custom_error_responses().cloned().unwrap())
        .build()?;

    // --- PASO 3: Aplicar el cambio de forma atómica ---
    println!("Promocionando entorno para la distribución: {}", distribution_id);
    
    client
        .update_distribution()
        .id(distribution_id)
        .distribution_config(updated_config)
        .if_match(etag) // Validamos con el ETag original
        .send()
        .await?;

    Ok(())
}
