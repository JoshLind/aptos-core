// Copyright © Aptos Foundation

use std::{error::Error, io::Cursor};

use chrono::Utc;
use diesel::{
    r2d2::{ConnectionManager, PooledConnection},
    PgConnection, QueryDsl, RunQueryDsl,
};
use hyper::{header, HeaderMap};
use image::{
    imageops::{resize, FilterType},
    DynamicImage, ImageBuffer, ImageFormat, ImageOutputFormat,
};
use reqwest::Client;

use serde_json::Value;

use crate::{
    db::upsert_uris,
    models::{NFTMetadataCrawlerEntry, NFTMetadataCrawlerURIs},
    schema::nft_metadata_crawler_uris,
};

pub struct Parser {
    pub entry: NFTMetadataCrawlerEntry,
    model: NFTMetadataCrawlerURIs,
    format: ImageFormat,
    target_size: (u32, u32),
    bucket: String,
    auth: String,
    force: bool,
}

impl Parser {
    pub fn new(
        e: NFTMetadataCrawlerEntry,
        ts: Option<(u32, u32)>,
        au: String,
        b: String,
        f: bool,
    ) -> Self {
        Self {
            model: NFTMetadataCrawlerURIs {
                token_uri: e.token_uri.clone(),
                raw_image_uri: None,
                cdn_json_uri: None,
                cdn_image_uri: None,
                image_resizer_retry_count: 0,
                json_parser_retry_count: 0,
                last_updated: Utc::now().naive_utc(),
            },
            entry: e,
            format: ImageFormat::Jpeg,
            target_size: ts.unwrap_or((400, 400)),
            bucket: b,
            auth: au,
            force: f,
        }
    }

    pub async fn parse(
        &mut self,
        conn: &mut PooledConnection<ConnectionManager<PgConnection>>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        if nft_metadata_crawler_uris::table
            .find(&self.entry.token_uri)
            .first::<NFTMetadataCrawlerURIs>(conn)
            .is_ok()
        {
            if self.force {
                self.log("Found URIs entry but forcing parse");
            } else {
                self.log("Skipping URI parse");
                return Ok(());
            }
        }

        match self.parse_json().await {
            Ok(json) => {
                self.log("Successfully parsed JSON");
                match self.write_json_to_gcs(json).await {
                    Ok(_) => self.log("Successfully saved JSON"),
                    Err(e) => self.log(&e.to_string()),
                }
            },
            Err(e) => {
                self.model.json_parser_retry_count += 1;
                self.log(&e.to_string())
            },
        }

        match upsert_uris(conn, self.model.clone()) {
            Ok(_) => self.log("Successfully upserted JSON URIs"),
            Err(e) => self.log(&e.to_string()),
        }

        match self.optimize_image().await {
            Ok(new_img) => {
                self.log("Successfully optimized image");
                match self.write_image_to_gcs(new_img).await {
                    Ok(_) => self.log("Successfully saved image"),
                    Err(e) => self.log(&e.to_string()),
                }
            },
            Err(e) => {
                self.model.image_resizer_retry_count += 1;
                self.log(&e.to_string())
            },
        }

        match upsert_uris(conn, self.model.clone()) {
            Ok(_) => self.log("Successfully upserted image URIs"),
            Err(e) => self.log(&e.to_string()),
        }

        Ok(())
    }

    async fn parse_json(&mut self) -> Result<Value, Box<dyn Error + Send + Sync>> {
        for _ in 0..3 {
            self.log(&format!(
                "Sending request for token_uri {}",
                self.entry.token_uri
            ));
            let response = reqwest::get(&self.entry.token_uri).await?;
            let parsed_json = response.json::<Value>().await?;
            if let Some(img) = parsed_json["image"].as_str() {
                self.model.raw_image_uri = Some(img.to_string());
                self.model.last_updated = Utc::now().naive_local();
            }
            return Ok(parsed_json);
        }
        Err("Error sending request x3, skipping JSON".into())
    }

    async fn write_json_to_gcs(&mut self, json: Value) -> Result<(), Box<dyn Error + Send + Sync>> {
        let client = Client::new();
        let url = format!(
            "https://storage.googleapis.com/upload/storage/v1/b/{}/o?uploadType=media&name={}",
            self.bucket,
            format!("json_{}.json", self.entry.token_data_id)
        );
        let json_string = json.to_string();

        let res = client
            .post(url)
            .bearer_auth(self.auth.clone())
            .header("Content-Type", "application/json")
            .body(json_string)
            .send()
            .await?;

        match res.status().as_u16() {
            200..=299 => Ok(()),
            _ => {
                let text = res.text().await?;
                Err(format!("Error saving JSON to GCS {}", text).into())
            },
        }
    }

    async fn optimize_image(&mut self) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
        for _ in 0..3 {
            let img_uri = self
                .model
                .raw_image_uri
                .clone()
                .unwrap_or(self.model.token_uri.clone());

            self.log(&format!(
                "Sending request for raw_image_uri {}",
                img_uri.clone()
            ));

            let response = reqwest::get(img_uri.clone()).await?;
            if response.status().is_success() {
                let img_bytes = response.bytes().await?;
                self.model.raw_image_uri = Some(img_uri);
                let format = image::guess_format(img_bytes.as_ref())?;
                self.format = format;
                match format {
                    ImageFormat::Gif | ImageFormat::Avif => return Ok(img_bytes.to_vec()),
                    _ => match image::load_from_memory(&img_bytes) {
                        Ok(img) => {
                            return Ok(self.to_bytes(resize(
                                &img.to_rgb8(),
                                self.target_size.0 as u32,
                                self.target_size.1 as u32,
                                FilterType::Gaussian,
                            ))?)
                        },
                        Err(e) => {
                            return Err(format!("Error converting image to bytes: {}", e).into());
                        },
                    },
                }
            }
        }
        Err("Error sending request x3, skipping image".into())
    }

    async fn write_image_to_gcs(
        &mut self,
        buffer: Vec<u8>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let client = Client::new();
        let mut headers = HeaderMap::new();

        let extension = match self.format {
            ImageFormat::Gif | ImageFormat::Avif => self
                .format
                .extensions_str()
                .last()
                .unwrap_or(&"gif")
                .to_string(),
            _ => "jpeg".to_string(),
        };

        let url = format!(
            "https://storage.googleapis.com/upload/storage/v1/b/{}/o?uploadType=media&name={}",
            "nftnft",
            format!("image_{}.{}", self.entry.token_data_id, extension)
        );

        headers.insert(
            header::CONTENT_TYPE,
            format!("image/{}", extension).parse().unwrap(),
        );
        headers.insert(
            header::CONTENT_LENGTH,
            buffer.len().to_string().parse().unwrap(),
        );

        let res = client
            .post(&url)
            .bearer_auth(self.auth.to_string())
            .headers(headers)
            .body(buffer)
            .send()
            .await?;

        match res.status().as_u16() {
            200..=299 => Ok(()),
            _ => {
                let text = res.text().await?;
                Err(format!("Error saving image to GCS {}", text).into())
            },
        }
    }

    fn to_bytes(
        &self,
        image_buffer: ImageBuffer<image::Rgb<u8>, Vec<u8>>,
    ) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
        let dynamic_image = DynamicImage::ImageRgb8(image_buffer);
        let mut byte_store = Cursor::new(Vec::new());
        match dynamic_image.write_to(&mut byte_store, ImageOutputFormat::Jpeg(50)) {
            Ok(_) => Ok(byte_store.into_inner()),
            Err(_) => Err("Error converting image to bytes".into()),
        }
    }

    fn log(&self, message: &str) {
        println!(
            "Transaction Version {}: {}",
            self.entry.last_transaction_version, message
        );
    }
}
