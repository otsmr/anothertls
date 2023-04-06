/*
 * Copyright (c) 2023, Tobias Müller <git@tsmr.eu>
 *
 */

#![allow(dead_code)]

use crate::{
    crypto::{ellipticcurve::Signature, CipherSuite},
    hash::{sha256::Sha256, sha384::Sha384, TranscriptHash},
    net::{
        alert::{AlertLevel, TlsError},
        extensions::{shared::SignatureScheme, ServerExtensions},
        handshake::{
            get_finished_handshake, get_verify_client_finished, Certificate, ClientHello,
            Handshake, HandshakeType, ServerHello,
        },
        key_schedule::KeySchedule,
        record::{Record, RecordPayloadProtection, RecordType, Value},
    },
    rand::{RngCore, URandomRng},
    utils::{bytes, keylog::KeyLog, log},
    TlsConfig,
};
use ibig::IBig;

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    result::Result,
};

#[derive(PartialEq, PartialOrd, Clone, Copy, Debug)]
#[repr(u8)]
enum HandshakeState {
    ClientHello,
    ClientCertificate = 0x10,
    ClientCertificateVerify,
    FinishWithError(TlsError),
    Finished,
    Ready,
}

pub struct TlsStream<'a> {
    stream: TcpStream,
    addr: SocketAddr,
    config: &'a TlsConfig,
    protection: Option<RecordPayloadProtection>,
    state: HandshakeState,
    key_log: Option<KeyLog>,
    client_cert: Option<Certificate>,
    certificate_request_context: Option<Vec<u8>>,
    rng: Box<dyn RngCore<IBig>>,
    tshash: Option<Box<dyn TranscriptHash>>,
    tshash_clienthello_serverfinished: Option<Box<dyn TranscriptHash>>,
}

impl<'a> TlsStream<'a> {
    pub fn new(stream: TcpStream, addr: SocketAddr, config: &'a TlsConfig) -> Self {
        Self {
            stream,
            addr,
            config,
            state: HandshakeState::ClientHello,
            key_log: None,
            client_cert: None,
            certificate_request_context: None,
            protection: None,
            rng: Box::new(URandomRng::new()),
            tshash: None,
            tshash_clienthello_serverfinished: None,
        }
    }
    pub fn read_to_end(&mut self) -> Result<(), TlsError> {
        // TODO: Read to end xD
        self.write_alert(TlsError::CloseNotify)
    }
    pub fn write_alert(&mut self, err: TlsError) -> Result<(), TlsError> {
        let data = vec![AlertLevel::get_from_error(err) as u8, err.as_u8()];

        let record = Record::new(RecordType::Alert, Value::Owned(data));

        let record_raw = if let Some(protect) = self.protection.as_mut() {
            protect.encrypt(record)?
        } else {
            record.as_bytes()
        };

        if self.stream.write_all(&record_raw).is_err() {
            return Err(TlsError::BrokenPipe);
        };

        Ok(())
    }

    pub fn do_handshake_block(&mut self) -> Result<(), TlsError> {
        if let Err(mut err) = self.do_handshake() {
            if err < TlsError::NotOfficial {
                self.write_alert(err)?;
            }
            if let TlsError::GotAlert(err_code) = err {
                err = TlsError::new(err_code);
            }
            return Err(err);
        }

        Ok(())
    }

    fn handle_client_hello(
        &mut self,
        record: Record,
        tx_buf: &mut Vec<u8>,
    ) -> Result<(), TlsError> {
        if record.content_type != RecordType::Handshake {
            return Err(TlsError::UnexpectedMessage);
        }

        let handshake = Handshake::from_raw(record.fraqment.as_ref())?;

        if handshake.handshake_type != HandshakeType::ClientHello {
            return Err(TlsError::UnexpectedMessage);
        }

        let client_hello = ClientHello::from_raw(handshake.fraqment)?;

        // -- Server Hello --
        let server_hello =
            ServerHello::from_client_hello(&client_hello, &mut *self.rng, self.config)?;
        let handshake_raw = Handshake::to_raw(HandshakeType::ServerHello, server_hello.to_raw());

        let mut tshash: Box<dyn TranscriptHash> = match server_hello.cipher_suite {
            CipherSuite::TLS_AES_256_GCM_SHA384 => Box::new(Sha384::new()),
            CipherSuite::TLS_AES_128_GCM_SHA256 => Box::new(Sha256::new()),
            CipherSuite::TLS_CHACHA20_POLY1305_SHA256 => todo!(),
            _ => return Err(TlsError::InsufficientSecurity),
        };

        // Add ClientHello
        tshash.update(record.fraqment.as_ref());
        tshash.update(&handshake_raw);

        let mut record_raw = Record::to_raw(RecordType::Handshake, &handshake_raw);
        tx_buf.append(&mut record_raw);

        // -- Change Cipher Spec --
        let mut server_change_cipher_spec = vec![0x14, 0x03, 0x03, 0x00, 0x01, 0x01];
        tx_buf.append(&mut server_change_cipher_spec);

        // -- Handshake Keys Calc --
        let key_schedule =
            KeySchedule::from_handshake(tshash.as_ref(), &client_hello, &server_hello)?;

        if let Some(filepath) = &self.config.keylog {
            let key_log = KeyLog::new(filepath.to_owned(), client_hello.random);
            key_log.append_handshake_traffic_secrets(
                &key_schedule
                    .server_handshake_traffic_secret
                    .pseudo_random_key,
                &key_schedule
                    .client_handshake_traffic_secret
                    .pseudo_random_key,
            );
            self.key_log = Some(key_log);
        }

        self.protection = RecordPayloadProtection::new(key_schedule);

        if self.protection.is_none() {
            return Err(TlsError::InternalError);
        }

        let protect = self.protection.as_mut().unwrap();

        // -- ServerParameters --

        // > EncryptedExtensions
        let encrypted_extensions = ServerExtensions::new();
        let encrypted_extensions_raw = encrypted_extensions.to_raw();
        let handshake_raw =
            Handshake::to_raw(HandshakeType::EncryptedExtensions, encrypted_extensions_raw);
        tshash.update(&handshake_raw);
        let record = Record::new(RecordType::Handshake, Value::Ref(&handshake_raw));
        let mut encrypted_record_raw = protect.encrypt(record)?;
        log::debug!("<-- EncryptedExtensions");
        tx_buf.append(&mut encrypted_record_raw);

        // > Certificate Request

        if let Some(client_cert_ca) = &self.config.client_cert_ca {
            // prevent an attacker who has temporary access to the client's
            // private key from pre-computing valid CertificateVerify messages
            self.certificate_request_context = Some(self.rng.between_bytes(32));
            let certificate_request = client_cert_ca
                .get_certificate_request(self.certificate_request_context.as_ref().unwrap());

            let handshake_raw =
                Handshake::to_raw(HandshakeType::CertificateRequest, certificate_request);

            tshash.update(&handshake_raw);
            let record = Record::new(RecordType::Handshake, Value::Ref(&handshake_raw));

            let mut encrypted_record_raw = protect.encrypt(record)?;
            log::debug!("<-- CertificateRequest");
            tx_buf.append(&mut encrypted_record_raw);
        }

        // -- Server Certificate --

        let certificate_raw = self.config.cert.get_certificate_for_handshake();

        let handshake_raw = Handshake::to_raw(HandshakeType::Certificate, certificate_raw);

        tshash.update(&handshake_raw);
        let record = Record::new(RecordType::Handshake, Value::Ref(&handshake_raw));
        let mut encrypted_record_raw = protect.encrypt(record)?;
        log::debug!("<-- Certificate");
        tx_buf.append(&mut encrypted_record_raw);

        // -- Server Certificate Verify --

        let certificate_verify_raw = self
            .config
            .cert
            .get_certificate_verify_for_handshake(&self.config.privkey, tshash.as_ref())?;

        let handshake_raw =
            Handshake::to_raw(HandshakeType::CertificateVerify, certificate_verify_raw);

        tshash.update(&handshake_raw);
        let record = Record::new(RecordType::Handshake, Value::Ref(&handshake_raw));
        let mut encrypted_record_raw = protect.encrypt(record)?;
        log::debug!("<-- CertificateVerify");
        tx_buf.append(&mut encrypted_record_raw);

        // -- FINISHED --
        let handshake_raw = get_finished_handshake(
            server_hello.hash,
            &protect.key_schedule.server_handshake_traffic_secret,
            tshash.as_ref(),
        )?;

        tshash.update(&handshake_raw);
        let record = Record::new(RecordType::Handshake, Value::Ref(&handshake_raw));
        let mut encrypted_record_raw = protect.encrypt(record)?;
        tx_buf.append(&mut encrypted_record_raw);

        if self.config.client_cert_ca.is_some() {
            self.state = HandshakeState::ClientCertificate;
        } else {
            self.state = HandshakeState::Finished;
        }
        self.tshash = Some(tshash);
        Ok(())
    }

    fn handle_handshake_encrypted_record(&mut self, record: Record) -> Result<(), TlsError> {
        log::debug!("==> Encrypted handshake record");

        let mut verify_data = None;
        let protection = self.protection.as_mut().unwrap();

        match self.state {
            // TODO: How to write this using if instead of match?
            HandshakeState::Finished | HandshakeState::FinishWithError(_) => {
                if self.tshash.is_none() {
                    return Err(TlsError::InternalError);
                }
                verify_data = Some(get_verify_client_finished(
                    &protection.key_schedule.client_handshake_traffic_secret,
                    self.tshash.as_ref().unwrap().as_ref(),
                )?);
            }
            _ => (),
        }

        let record = protection.decrypt(record)?;

        if record.content_type != RecordType::Handshake
            || (self.config.client_cert_ca.is_some() && self.certificate_request_context.is_none())
        {
            if record.content_type == RecordType::Alert {
                return Err(TlsError::GotAlert(record.fraqment.as_ref()[1]));
            }
            return Err(TlsError::UnexpectedMessage);
        }

        let handshake = Handshake::from_raw(record.fraqment.as_ref())?;

        match self.state {
            HandshakeState::ClientCertificate => {
                if handshake.handshake_type != HandshakeType::Certificate {
                    return Err(TlsError::UnexpectedMessage);
                }

                log::debug!("--> ClientCertificate");

                self.tshash_clienthello_serverfinished =
                    Some((*self.tshash.as_ref().unwrap()).clone());

                self.tshash
                    .as_mut()
                    .unwrap()
                    .update(record.fraqment.as_ref());

                let mut consumed = 1;
                let cert_request_context_len = handshake.fraqment[0] as usize;
                let cert_request_context = &handshake.fraqment[1..cert_request_context_len + 1];

                if cert_request_context != self.certificate_request_context.as_ref().unwrap() {
                    return Err(TlsError::HandshakeFailure);
                }

                consumed += cert_request_context_len;

                let certs_len =
                    bytes::to_u128_le_fill(&handshake.fraqment[consumed..consumed + 3]) as usize;
                consumed += 3;

                if certs_len == 0 {
                    log::debug!("Client send no certificate!");
                    self.state = HandshakeState::FinishWithError(TlsError::CertificateRequired);
                    return Ok(());
                }


                let cert_len =
                    bytes::to_u128_le_fill(&handshake.fraqment[consumed..consumed + 3]) as usize;
                consumed += 3;

                if certs_len != cert_len + 5 {
                    todo!("Add support for multiple certs");
                }

                let cert = Certificate::from_raw_x509(
                    handshake.fraqment[consumed..consumed + cert_len].to_vec(),
                )?;

                if !cert
                    .x509
                    .as_ref()
                    .unwrap()
                    .tbs_certificate
                    .validity
                    .is_valid()
                {
                    log::debug!("Certificate is not valid");
                    self.state = HandshakeState::FinishWithError(TlsError::CertificateExpired);
                    return Ok(());
                }

                log::debug!("Client certificate:");
                // TODO: only in debug
                let issuer = &cert.x509.as_ref().unwrap().tbs_certificate.issuer;
                let subject = &cert.x509.as_ref().unwrap().tbs_certificate.subject;

                log::debug!("   subject: {subject}");
                log::debug!("   issuer: {issuer}");

                if let Some(f) = self.config.client_cert_custom_verify_fn.as_ref() {
                    if !f(cert.x509.as_ref().unwrap()) {
                        log::debug!("Certificate denied by custom verify function");
                        self.state = HandshakeState::FinishWithError(TlsError::AccessDenied);
                        return Ok(());
                    }
                }

                self.client_cert = Some(cert);

                self.state = HandshakeState::ClientCertificateVerify;
            }
            HandshakeState::ClientCertificateVerify => {
                log::debug!("--> ClientCertificateVerify");

                if self.client_cert.is_none() {
                    return Err(TlsError::UnexpectedMessage);
                }

                let algo = SignatureScheme::new(bytes::to_u16(&handshake.fraqment[0..2]))?;

                let mut consumed = 4; // algo and len

                match algo {
                    SignatureScheme::ecdsa_secp256r1_sha256 => {
                        let (signature, size) =
                            match Signature::from_der(&handshake.fraqment[consumed..]) {
                                Ok(e) => e,
                                Err(e) => {
                                    self.state = HandshakeState::FinishWithError(e);
                                    return Ok(());
                                }
                            };

                        consumed += size;

                        if self
                            .client_cert
                            .as_ref()
                            .unwrap()
                            .verify_client_certificate(
                                signature,
                                self.tshash.as_ref().unwrap().as_ref(),
                            )
                            .is_err()
                        {
                            self.state = HandshakeState::FinishWithError(TlsError::BadCertificate);
                            return Ok(());
                        }
                    }
                    e => todo!("SignatureScheme {e:?} for client cert not implemented yet"),
                }

                let sign_len = bytes::to_u16(&handshake.fraqment[2..4]) as usize;
                if sign_len != consumed - 4 || self.client_cert.is_none() {
                    self.state = HandshakeState::FinishWithError(TlsError::BadCertificate);
                    return Ok(());
                }

                // TODO: Check the validity of the client cert

                // Validate client cert against the CA
                if self
                    .config
                    .client_cert_ca
                    .as_ref()
                    .unwrap()
                    .has_signed(self.client_cert.as_ref().unwrap())
                    .is_err()
                {
                    self.state = HandshakeState::FinishWithError(TlsError::UnknownCa)
                }

                self.tshash
                    .as_mut()
                    .unwrap()
                    .update(record.fraqment.as_ref());

                log::debug!("Certificate is valid.");

                self.state = HandshakeState::Finished;
            }
            HandshakeState::Finished | HandshakeState::FinishWithError(_) => {
                log::debug!("--> Finished");

                if verify_data.is_none() {
                    return Err(TlsError::UnexpectedMessage);
                }
                if handshake.fraqment != verify_data.unwrap() {
                    return Err(TlsError::DecryptError);
                }

                // Derive-Secret: ClientHello..server Finished
                if self.tshash_clienthello_serverfinished.is_some() {
                    protection.generate_application_keys(
                        self.tshash_clienthello_serverfinished
                            .as_ref()
                            .unwrap()
                            .as_ref(),
                    )?;
                } else {
                    protection.generate_application_keys(self.tshash.as_ref().unwrap().as_ref())?;
                }

                if let Some(k) = &self.key_log {
                    k.append_application_traffic_secrets(
                        &protection
                            .application_keys
                            .as_ref()
                            .unwrap()
                            .server
                            .traffic_secret,
                        &protection
                            .application_keys
                            .as_ref()
                            .unwrap()
                            .client
                            .traffic_secret,
                    );
                }

                if let HandshakeState::FinishWithError(err) = self.state {
                    return Err(err);
                }
                self.state = HandshakeState::Ready;
            }
            _ => (),
        }
        Ok(())
    }

    fn handle_handshake_record(&mut self, record: Record) -> Result<Option<Vec<u8>>, TlsError> {
        if record.content_type == RecordType::ChangeCipherSpec {
            log::debug!("--> ChangeCipherSpec");
            if self.state == HandshakeState::ClientHello {
                return Err(TlsError::UnexpectedMessage);
            }
            return Ok(None);
        }

        let mut tx_buf = Vec::with_capacity(4096);

        match self.state {
            HandshakeState::Ready => {}
            HandshakeState::ClientHello => {
                log::debug!("--> ClientHello");
                self.handle_client_hello(record, &mut tx_buf)?;
            }
            state if state >= HandshakeState::ClientCertificate => {
                self.handle_handshake_encrypted_record(record)?;
            }
            _ => (),
        }
        Ok(Some(tx_buf))
    }
    fn do_handshake(&mut self) -> Result<(), TlsError> {
        let mut rx_buf: [u8; 4096] = [0; 4096];

        while self.state != HandshakeState::Ready {
            let n = match self.stream.read(&mut rx_buf) {
                Ok(n) => n,
                Err(_) => return Err(TlsError::DecodeError),
            };
            let mut consumed_total = 0;

            while consumed_total < n {
                let (consumed, record) = Record::from_raw(&rx_buf[consumed_total..n])?;
                consumed_total += consumed;

                let tx_buf = self.handle_handshake_record(record)?;

                // Send buffer
                if tx_buf.is_some()
                    && !tx_buf.as_ref().unwrap().is_empty()
                    && self.stream.write_all(tx_buf.unwrap().as_slice()).is_err()
                {
                    return Err(TlsError::BrokenPipe);
                }
            }
            rx_buf.fill(0);
        }

        Ok(())
    }

    pub fn read<'b>(&'b mut self, buf: &'b mut [u8]) -> Result<usize, TlsError> {
        let mut rx_buf: [u8; 4096] = [0; 4096];

        let n = match self.stream.read(&mut rx_buf) {
            Ok(n) => n,
            Err(_) => return Err(TlsError::BrokenPipe),
        };

        let (_consumed, record) = Record::from_raw(&rx_buf[..n])?;

        if record.len != record.fraqment.len() {
            return Err(TlsError::DecodeError);
        }

        let record = self.protection.as_mut().unwrap().decrypt(record)?;

        if record.content_type != RecordType::ApplicationData {
            todo!();
        }
        if record.len > buf.len() {
            todo!("Handle records bigger than the buf.len()");
        }
        for (i, b) in record.fraqment.as_ref().iter().enumerate() {
            buf[i] = *b;
        }
        Ok(record.fraqment.len())
    }

    pub fn write_all<'b>(&'b mut self, src: &'b [u8]) -> Result<(), TlsError> {
        let record = Record::new(RecordType::ApplicationData, Value::Ref(src));

        let record = self.protection.as_mut().unwrap().encrypt(record)?;

        if self.stream.write_all(&record).is_err() {
            return Err(TlsError::BrokenPipe);
        };

        Ok(())
    }
}

// TODO: create tests
