//! Ported verbatim from the former `web/overlay.lang.js`, whose table was
//! generated from jellyfin-web/src/strings/.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Strings {
    pub server_host: &'static str,
    pub server_host_help: &'static str,
    pub connect: &'static str,
    pub connection_failure: &'static str,
    pub unable_to_connect: &'static str,
    pub got_it: &'static str,
    pub undo: &'static str,
    pub redo: &'static str,
    pub cut: &'static str,
    pub copy: &'static str,
    pub paste: &'static str,
    pub select_all: &'static str,
}

/// The edit menu's labels for one language.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EditEntry {
    pub lang: &'static str,
    pub undo: &'static str,
    pub redo: &'static str,
    pub cut: &'static str,
    pub copy: &'static str,
    pub paste: &'static str,
    pub select_all: &'static str,
}

/// Resolved per language, falling back to [`FALLBACK_LANGUAGE`].
pub const EDIT_LANGUAGES: &[EditEntry] = &[EditEntry {
    lang: "en-us",
    undo: "Undo",
    redo: "Redo",
    cut: "Cut",
    copy: "Copy",
    paste: "Paste",
    select_all: "Select All",
}];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry {
    pub lang: &'static str,
    pub header_connect_to_server: Option<&'static str>,
    pub label_server_host: Option<&'static str>,
    pub label_server_host_help: Option<&'static str>,
    pub connect: Option<&'static str>,
    pub header_connection_failure: Option<&'static str>,
    pub message_unable_to_connect_to_server: Option<&'static str>,
    pub button_got_it: Option<&'static str>,
}

pub const FALLBACK_LANGUAGE: &str = "en-us";

pub const LANGUAGES: &[Entry] = &[
    Entry {
        lang: "af",
        header_connect_to_server: Some("Konnekteer aan Bediener"),
        label_server_host: None,
        label_server_host_help: None,
        connect: Some("Konnekteer"),
        header_connection_failure: Some("Konneksie Fout"),
        message_unable_to_connect_to_server: None,
        button_got_it: Some("Het Dit So"),
    },
    Entry {
        lang: "ar",
        header_connect_to_server: Some("اتصل إلى الخادم"),
        label_server_host: Some("المضيف"),
        label_server_host_help: Some("192.168.1.100:8096 أو https://myserver.com"),
        connect: Some("إتصال"),
        header_connection_failure: Some("فشل في الاتصال"),
        message_unable_to_connect_to_server: Some(
            "لم نستطع الاتصال إلى الخادم المختار في الوقت الحالي. الرجاء التأكد من أنه يعمل ثم المحاولة مرة أخرى.",
        ),
        button_got_it: Some("حسنا"),
    },
    Entry {
        lang: "as",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: None,
    },
    Entry {
        lang: "be-by",
        header_connect_to_server: Some("Падлучыцца да сервера"),
        label_server_host: Some("Вядучы"),
        label_server_host_help: Some("192.168.1.100:8096 або https://myserver.com"),
        connect: Some("Падлучыцца"),
        header_connection_failure: Some("Збой падлучэння"),
        message_unable_to_connect_to_server: Some(
            "Мы не можам зараз падключыцца да выбранага сервера. Упэўніцеся, што ён запушчаны, і паўтарыце спробу.",
        ),
        button_got_it: Some("Зразумела"),
    },
    Entry {
        lang: "bg-bg",
        header_connect_to_server: Some("Свържи се със сървър"),
        label_server_host: Some("Хост"),
        label_server_host_help: Some("192.168.1.100:8096 или https://myserver.com"),
        connect: Some("Свързване"),
        header_connection_failure: Some("Проблем при свързване"),
        message_unable_to_connect_to_server: Some(
            "В момента не можем да се свържем с избрания сървър. Моля, уверете се, че работи и опитайте отново.",
        ),
        button_got_it: Some("Добре"),
    },
    Entry {
        lang: "bn",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: None,
    },
    Entry {
        lang: "bn_BD",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: Some("কানেক্ট"),
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: Some("বুঝেছি"),
    },
    Entry {
        lang: "ca",
        header_connect_to_server: Some("Connectar al servidor"),
        label_server_host: Some("Amfitrió"),
        label_server_host_help: Some("192.168.1.100:8096 o https://myserver.com"),
        connect: Some("Connecta"),
        header_connection_failure: Some("Error de connexió"),
        message_unable_to_connect_to_server: Some(
            "No es pot connectar amb el servidor seleccionat en aquest moment. Assegureu-vos que està funcionant i torni a intentar-ho.",
        ),
        button_got_it: Some("Entesos"),
    },
    Entry {
        lang: "ch",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: None,
    },
    Entry {
        lang: "cs",
        header_connect_to_server: Some("Připojit k serveru"),
        label_server_host: Some("Host"),
        label_server_host_help: Some("192.168.1.100:8096 nebo https://mujserver.cz"),
        connect: Some("Připojit"),
        header_connection_failure: Some("Připojení selhalo"),
        message_unable_to_connect_to_server: Some(
            "Nejsme schopni se připojit k vybranému serveru právě teď. Prosím, ujistěte se, že je spuštěn a zkuste to znovu.",
        ),
        button_got_it: Some("Rozumím"),
    },
    Entry {
        lang: "cy",
        header_connect_to_server: None,
        label_server_host: Some("Lletywr"),
        label_server_host_help: None,
        connect: Some("Cysylltu"),
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: Some("Dyna Fe"),
    },
    Entry {
        lang: "da",
        header_connect_to_server: Some("Forbind til server"),
        label_server_host: Some("Vært"),
        label_server_host_help: Some("F. eks: 192.168.1.100:8096 eller https://myserver.com"),
        connect: Some("Forbind"),
        header_connection_failure: Some("Forbindelsesfejl"),
        message_unable_to_connect_to_server: Some(
            "Vi kan ikke forbinde til den valgte server på nuværende tidspunkt. Sikrer dig venligst at serveren kører og prøv igen.",
        ),
        button_got_it: Some("Forstået"),
    },
    Entry {
        lang: "de",
        header_connect_to_server: Some("Mit Server verbinden"),
        label_server_host: Some("Adresse"),
        label_server_host_help: Some("192.168.1.100:8096 oder https://myserver.com"),
        connect: Some("Verbinden"),
        header_connection_failure: Some("Verbindungsfehler"),
        message_unable_to_connect_to_server: Some(
            "Wir können gerade keine Verbindung zum gewählten Server herstellen. Bitte stelle sicher, dass dieser läuft und versuche es erneut.",
        ),
        button_got_it: Some("Verstanden"),
    },
    Entry {
        lang: "el",
        header_connect_to_server: Some("Σύνδεση στον Διακομιστή"),
        label_server_host: Some("Host"),
        label_server_host_help: Some("192.168.1.100:8096 ή https://myserver.com"),
        connect: Some("Σύνδεση"),
        header_connection_failure: Some("Αποτυχία σύνδεσης"),
        message_unable_to_connect_to_server: Some(
            "Δεν είναι δυνατή η σύνδεση με τον επιλεγμένο διακομιστή αυτή τη στιγμή. Βεβαιωθείτε ότι εκτελείται και προσπαθήστε ξανά.",
        ),
        button_got_it: Some("Το κατάλαβα"),
    },
    Entry {
        lang: "en-gb",
        header_connect_to_server: Some("Connect to Server"),
        label_server_host: Some("Server Address"),
        label_server_host_help: Some("192.168.1.100:8096 or https://myserver.com"),
        connect: Some("Connect"),
        header_connection_failure: Some("Connection Failure"),
        message_unable_to_connect_to_server: Some(
            "We're unable to connect to the selected server right now. Please ensure it is running and try again.",
        ),
        button_got_it: Some("Got It"),
    },
    Entry {
        lang: "en-us",
        header_connect_to_server: Some("Connect to Server"),
        label_server_host: Some("Server Address"),
        label_server_host_help: Some("192.168.1.100:8096 or https://myserver.com"),
        connect: Some("Connect"),
        header_connection_failure: Some("Connection Failure"),
        message_unable_to_connect_to_server: Some(
            "We're unable to connect to the selected server right now. Please ensure it is running and try again.",
        ),
        button_got_it: Some("Got It"),
    },
    Entry {
        lang: "eo",
        header_connect_to_server: Some("Konekti al Servilo"),
        label_server_host: Some("Gastigo"),
        label_server_host_help: Some("192.168.1.100:8096 aŭ https://myserver.com"),
        connect: Some("Konektu"),
        header_connection_failure: Some("Konekto Malsukcesis"),
        message_unable_to_connect_to_server: Some(
            "Ni ne povas konektiĝi al la elektita servilo nun. Certigi, ke ĝi funkcias kaj provi denove.",
        ),
        button_got_it: Some("Kompreneblas"),
    },
    Entry {
        lang: "es-ar",
        header_connect_to_server: Some("Conectar al servidor"),
        label_server_host: Some("Host"),
        label_server_host_help: Some("192.168.1.100:8096 o https://miservidor.com"),
        connect: Some("Conectar"),
        header_connection_failure: Some("Conexión fallida"),
        message_unable_to_connect_to_server: Some(
            "No podemos conectarnos al servidor seleccionado en este momento. Asegúrese de que se esté ejecutando e intente nuevamente.",
        ),
        button_got_it: Some("Lo entendí"),
    },
    Entry {
        lang: "es-mx",
        header_connect_to_server: Some("Conectarse al servidor"),
        label_server_host: Some("Servidor"),
        label_server_host_help: Some("192.168.1.100:8096 o https://miservidor.com"),
        connect: Some("Conectar"),
        header_connection_failure: Some("Falla de conexión"),
        message_unable_to_connect_to_server: Some(
            "No podemos conectarnos al servidor seleccionado en este momento. Por favor, asegúrate de que está funcionando e inténtalo de nuevo.",
        ),
        button_got_it: Some("Hecho"),
    },
    Entry {
        lang: "es",
        header_connect_to_server: Some("Conectar al servidor"),
        label_server_host: Some("Host"),
        label_server_host_help: Some("192.168.1.100:8096 o https://miservidor.com"),
        connect: Some("Conectar"),
        header_connection_failure: Some("Fallo de conexión"),
        message_unable_to_connect_to_server: Some(
            "No podemos conectar con el servidor seleccionado ahora mismo. Por favor, asegúrate de que esta funcionando e inténtalo otra vez.",
        ),
        button_got_it: Some("Entendido"),
    },
    Entry {
        lang: "es_419",
        header_connect_to_server: Some("Conectarse al servidor"),
        label_server_host: Some("Servidor"),
        label_server_host_help: Some("192.168.1.100:8096 o https://miservidor.com"),
        connect: Some("Conectar"),
        header_connection_failure: Some("Falla de conexión"),
        message_unable_to_connect_to_server: Some(
            "No podemos conectarnos al servidor seleccionado en este momento. Por favor, asegúrate de que está funcionando e inténtalo de nuevo.",
        ),
        button_got_it: Some("Hecho"),
    },
    Entry {
        lang: "es_DO",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: None,
    },
    Entry {
        lang: "et",
        header_connect_to_server: Some("Ühendu serveriga"),
        label_server_host: Some("Peremeesmasin"),
        label_server_host_help: Some("192.168.1.100:8096 või https://myserver.com"),
        connect: Some("Ühenda"),
        header_connection_failure: Some("Ühenduse tõrge"),
        message_unable_to_connect_to_server: Some(
            "Me ei saa praegu valitud serveriga ühendust. Veendu, et see töötab ja proovi uuesti.",
        ),
        button_got_it: Some("Selge"),
    },
    Entry {
        lang: "eu",
        header_connect_to_server: Some("Zerbitzariari konektatu"),
        label_server_host: Some("Host"),
        label_server_host_help: Some("192.168.1.100: 8096 edo https://miservidor.com"),
        connect: Some("Konektatu"),
        header_connection_failure: Some("Konexio-akatsa"),
        message_unable_to_connect_to_server: Some(
            "Ezin dugu une honetan hautatutako zerbitzariarekin konektatu. Mesedez, ziurtatu funtzionatzen ari dela eta saiatu berriro.",
        ),
        button_got_it: Some("Ulertua"),
    },
    Entry {
        lang: "fa",
        header_connect_to_server: Some("اتصال به سرور"),
        label_server_host: Some("میزبان"),
        label_server_host_help: Some("192.168.1.100:8096 یا https://myserver.com"),
        connect: Some("اتصال"),
        header_connection_failure: Some("عدم اتصال"),
        message_unable_to_connect_to_server: Some(""),
        button_got_it: Some("متوجه شدم"),
    },
    Entry {
        lang: "fi",
        header_connect_to_server: Some("Yhdistä palvelimeen"),
        label_server_host: Some("Isäntä"),
        label_server_host_help: Some("192.168.1.100:8096 tai https://myserver.com"),
        connect: Some("Yhdistä"),
        header_connection_failure: Some("Yhteys epäonnistui"),
        message_unable_to_connect_to_server: Some(
            "Valittuun palvelimeen yhdistäminen epäonnistui. Tarkista, että se on päällä ja yritä uudestaan.",
        ),
        button_got_it: Some("Selvä"),
    },
    Entry {
        lang: "fil",
        header_connect_to_server: Some("Kumonekta sa Server"),
        label_server_host: Some("Host"),
        label_server_host_help: Some("192.168.1.100:8096 o https://myserver.com"),
        connect: Some("Kumonekta"),
        header_connection_failure: Some("Nag-fail ang koneksyon"),
        message_unable_to_connect_to_server: Some(
            "Hindi kami makakonekta sa napiling server sa ngayon. Pakitiyak na ito ay tumatakbo at subukang muli.",
        ),
        button_got_it: Some("Nakuha ko"),
    },
    Entry {
        lang: "fo",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: None,
    },
    Entry {
        lang: "fr-ca",
        header_connect_to_server: Some("Connexion au serveur"),
        label_server_host: Some("Hôte"),
        label_server_host_help: Some("192.168.1.100:8096 ou https://monserveur.com"),
        connect: Some("Connexion"),
        header_connection_failure: Some("Échec de connexion"),
        message_unable_to_connect_to_server: Some(
            "Impossible de se connecter au serveur sélectionné. Assurez-vous qu'il est opérationnel.",
        ),
        button_got_it: Some("J'ai compris"),
    },
    Entry {
        lang: "fr",
        header_connect_to_server: Some("Connexion au serveur"),
        label_server_host: Some("Nom d'hôte"),
        label_server_host_help: Some("192.168.1.1:8096 ou https://monserveur.com"),
        connect: Some("Se connecter"),
        header_connection_failure: Some("Échec de connexion"),
        message_unable_to_connect_to_server: Some(
            "Nous sommes dans l'impossibilité de nous connecter au serveur sélectionné. Veuillez vérifier qu'il est opérationnel et réessayez.",
        ),
        button_got_it: Some("Compris"),
    },
    Entry {
        lang: "ga",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: None,
    },
    Entry {
        lang: "gl",
        header_connect_to_server: Some("Conectar ao Servidor"),
        label_server_host: None,
        label_server_host_help: None,
        connect: Some("Conectar"),
        header_connection_failure: Some("Fallo de Conexión"),
        message_unable_to_connect_to_server: None,
        button_got_it: Some("Entendo"),
    },
    Entry {
        lang: "gsw",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: None,
    },
    Entry {
        lang: "gu",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: None,
    },
    Entry {
        lang: "he",
        header_connect_to_server: Some("התחבר לשרת"),
        label_server_host: Some("מארח"),
        label_server_host_help: Some("192.168.1.100:8096 או https://myserver.com"),
        connect: Some("התחבר"),
        header_connection_failure: Some("כשל בחיבור"),
        message_unable_to_connect_to_server: None,
        button_got_it: Some("הבנתי"),
    },
    Entry {
        lang: "hi-in",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: Some("समझ गया"),
    },
    Entry {
        lang: "hr",
        header_connect_to_server: Some("Spoji se na Server"),
        label_server_host: Some("Domaćin"),
        label_server_host_help: Some("192.168.1.100:8096 ili https://myserver.com"),
        connect: Some("Povezati"),
        header_connection_failure: Some("Neuspjelo spajanje"),
        message_unable_to_connect_to_server: Some(
            "Nismo u mogućnosti spojiti se na odabrani poslužitelj. Provjerite dali je pokrenut i pokušajte ponovno.",
        ),
        button_got_it: Some("Shvaćam"),
    },
    Entry {
        lang: "hu",
        header_connect_to_server: Some("Kapcsolódás a Szerverhez"),
        label_server_host: Some("Kiszolgáló"),
        label_server_host_help: Some("192.168.1.100:8096 vagy https://myserver.com"),
        connect: Some("Kapcsolódás"),
        header_connection_failure: Some("Kapcsolathiba"),
        message_unable_to_connect_to_server: Some(
            "Jelenleg nem tudunk csatlakozni a kiválasztott szerverhez. Győződj meg róla, hogy fut és próbáld meg újra.",
        ),
        button_got_it: Some("Értettem"),
    },
    Entry {
        lang: "hy",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: Some("192.168.1.100:8096 կամ https://myserver.com"),
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: None,
    },
    Entry {
        lang: "id",
        header_connect_to_server: Some("Sambungkan ke server"),
        label_server_host: Some("Host"),
        label_server_host_help: Some("192.168.1.100:8096 atau https://myserver.com"),
        connect: Some("Sambung"),
        header_connection_failure: Some("Koneksi Bermasalah"),
        message_unable_to_connect_to_server: Some(
            "Kami tidak dapat terhubung ke server yang dipilih sekarang. Harap pastikan itu berjalan dan coba lagi.",
        ),
        button_got_it: Some("Paham"),
    },
    Entry {
        lang: "is-is",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: Some("Tengjast"),
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: Some("Skilið"),
    },
    Entry {
        lang: "it",
        header_connect_to_server: Some("Connettersi al Server"),
        label_server_host: Some("Host"),
        label_server_host_help: Some("192.168.1.100:8096 o https://myserver.com"),
        connect: Some("Connetti"),
        header_connection_failure: Some("Errore di connessione"),
        message_unable_to_connect_to_server: Some(
            "Non siamo in grado di connettersi al server selezionato al momento. Per favore assicurati che sia in esecuzione e riprova.",
        ),
        button_got_it: Some("Ho capito"),
    },
    Entry {
        lang: "ja",
        header_connect_to_server: Some("サーバーに接続"),
        label_server_host: Some("ホスト"),
        label_server_host_help: Some("192.168.1.100:8096 又は https://myserver.com"),
        connect: Some("接続"),
        header_connection_failure: Some("接続失敗"),
        message_unable_to_connect_to_server: Some(
            "現在、選択されたサーバーへの接続ができません。稼働していることを確認しもう一度やり直してください。",
        ),
        button_got_it: Some("了解"),
    },
    Entry {
        lang: "jbo",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: Some("je'e"),
    },
    Entry {
        lang: "ka",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: Some("გასაგებია"),
    },
    Entry {
        lang: "kab",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: None,
    },
    Entry {
        lang: "kk",
        header_connect_to_server: Some("Serverge qosylu"),
        label_server_host: Some("Tüiın"),
        label_server_host_help: Some("192.168.1.100:8096 nemese https://myserver.com"),
        connect: Some("Qosylu"),
        header_connection_failure: Some("Qosylu sätsız"),
        message_unable_to_connect_to_server: Some(
            "Tañdalğan serverge qosyluymyz däl qazır mümkın emes. Būl ıske qosylğanyna köz jetkızıñız jäne ärekettı keiın qaitalañyz.",
        ),
        button_got_it: Some("Tüsınıktı"),
    },
    Entry {
        lang: "kn",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: None,
    },
    Entry {
        lang: "ko",
        header_connect_to_server: Some("서버 접속"),
        label_server_host: Some("호스트"),
        label_server_host_help: Some("192.168.1.100:8096 또는 https://myserver.com"),
        connect: Some("접속"),
        header_connection_failure: Some("연결 실패"),
        message_unable_to_connect_to_server: Some(
            "선택한 서버에 연결할 수 없습니다. 서버가 실행 중인지 확인후 다시 시도하세요.",
        ),
        button_got_it: Some("알겠습니다"),
    },
    Entry {
        lang: "kw",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: None,
    },
    Entry {
        lang: "ky",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: None,
    },
    Entry {
        lang: "lt-lt",
        header_connect_to_server: Some("Prisijungti prie Serverio"),
        label_server_host: None,
        label_server_host_help: Some("192.168.1.100:8096 arba https://manoserveris.lt"),
        connect: Some("Prisijungti"),
        header_connection_failure: Some("Prisijungimo klaida"),
        message_unable_to_connect_to_server: None,
        button_got_it: Some("Supratau"),
    },
    Entry {
        lang: "lv",
        header_connect_to_server: Some("Pievienoties pie servera"),
        label_server_host: Some("Resursdators"),
        label_server_host_help: Some("192.168.1.100:8096 vai https://myserver.com"),
        connect: Some("Savienot"),
        header_connection_failure: Some("Savienojuma kļūda"),
        message_unable_to_connect_to_server: Some(
            "Mēs pašlaik nevaram sazināties ar izvēlēto serveri. Pārliecinies ka tas strādā, un mēģini vēlreiz.",
        ),
        button_got_it: Some("Sapratu"),
    },
    Entry {
        lang: "mg",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: None,
    },
    Entry {
        lang: "mk",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: Some("Поврзи"),
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: Some("Потврдувам"),
    },
    Entry {
        lang: "ml",
        header_connect_to_server: Some("സെർവറിലേക്ക് കണക്റ്റുചെയ്യുക"),
        label_server_host: Some("ഹോസ്റ്റ്"),
        label_server_host_help: Some("192.168.1.100:8096 അല്ലെങ്കിൽ https://myserver.com"),
        connect: Some("ബന്ധിപ്പിക്കുക"),
        header_connection_failure: Some("കണക്ഷൻ പരാജയം"),
        message_unable_to_connect_to_server: Some(
            "തിരഞ്ഞെടുത്ത സെർവറിലേക്ക് ഞങ്ങൾക്ക് ഇപ്പോൾ കണക്റ്റുചെയ്യാൻ കഴിയില്ല. ഇത് പ്രവർത്തിക്കുന്നുവെന്ന് ഉറപ്പാക്കി വീണ്ടും ശ്രമിക്കുക.",
        ),
        button_got_it: Some("മനസ്സിലായി"),
    },
    Entry {
        lang: "mn",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: None,
    },
    Entry {
        lang: "mr",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: Some("समजले"),
    },
    Entry {
        lang: "ms",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: Some("Sambung"),
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: Some("Terima"),
    },
    Entry {
        lang: "mt",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: None,
    },
    Entry {
        lang: "my",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: Some("ချိတ်ဆက်ပါ"),
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: Some("ရပြီ"),
    },
    Entry {
        lang: "nb",
        header_connect_to_server: Some("Koble til server"),
        label_server_host: Some("Vertsnavn"),
        label_server_host_help: Some("192.168.1.100:8096 eller https://minserver.no"),
        connect: Some("Koble til"),
        header_connection_failure: Some("Tilkobling feilet"),
        message_unable_to_connect_to_server: Some(
            "Vi klarte ikke å koble til den valgte serveren akkurat nå. Vennligst sørg for at den kjører og prøv på nytt.",
        ),
        button_got_it: Some("Skjønner"),
    },
    Entry {
        lang: "ne",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: None,
    },
    Entry {
        lang: "nl",
        header_connect_to_server: Some("Verbinden met server"),
        label_server_host: Some("Host"),
        label_server_host_help: Some("192.168.1.100:8096 of https://mijnserver.nl"),
        connect: Some("Verbinden"),
        header_connection_failure: Some("Verbindingsfout"),
        message_unable_to_connect_to_server: Some(
            "Het is momenteel niet mogelijk met de geselecteerde server te verbinden. Controleer of deze draait en probeer het opnieuw.",
        ),
        button_got_it: Some("Begrepen"),
    },
    Entry {
        lang: "nn",
        header_connect_to_server: Some("Kople til tenar"),
        label_server_host: None,
        label_server_host_help: None,
        connect: Some("Kople til"),
        header_connection_failure: Some("Tilkoplingsfeil"),
        message_unable_to_connect_to_server: None,
        button_got_it: Some("Skjønner"),
    },
    Entry {
        lang: "pa",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: Some("ਕਨੈਕਟ ਕਰੋ"),
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: None,
    },
    Entry {
        lang: "pl",
        header_connect_to_server: Some("Podłącz do serwera"),
        label_server_host: Some("Serwer"),
        label_server_host_help: Some("192.168.1.100:8096 lub https://mojserwer.pl"),
        connect: Some("Połącz"),
        header_connection_failure: Some("Niepowodzenie połączenia"),
        message_unable_to_connect_to_server: Some(
            "Połączenie z wybranym serwerem jest teraz niemożliwe. Upewnij się, że jest uruchomiony i spróbuj ponownie.",
        ),
        button_got_it: Some("Rozumiem"),
    },
    Entry {
        lang: "pr",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: Some("Aye-Aye"),
    },
    Entry {
        lang: "pt-br",
        header_connect_to_server: Some("Conectar ao Servidor"),
        label_server_host: Some("Servidor"),
        label_server_host_help: Some("192.168.1.100:8096 ou https://meuservidor.com"),
        connect: Some("Conectar"),
        header_connection_failure: Some("Falha na Conexão"),
        message_unable_to_connect_to_server: Some(
            "Não foi possível conectar ao servidor selecionado. Por favor, verifique se está sendo executado e tente novamente.",
        ),
        button_got_it: Some("Feito"),
    },
    Entry {
        lang: "pt-pt",
        header_connect_to_server: Some("Ligar ao servidor"),
        label_server_host: Some("Servidor"),
        label_server_host_help: Some("192.168.1.100:8096 ou https://omeudominio.com"),
        connect: Some("Ligar"),
        header_connection_failure: Some("Falha de ligação"),
        message_unable_to_connect_to_server: Some(
            "Não foi possível estabelecer ligação ao servidor. Por favor, certifique-se de que o servidor está a correr e tente de novo.",
        ),
        button_got_it: Some("Entendido"),
    },
    Entry {
        lang: "pt",
        header_connect_to_server: Some("Ligar ao Servidor"),
        label_server_host: Some("Servidor"),
        label_server_host_help: Some("192.168.1.100:8096 ou https://omeudominio.com"),
        connect: Some("Ligar"),
        header_connection_failure: Some("Falha de Ligação"),
        message_unable_to_connect_to_server: Some(
            "Não foi possível estabelecer ligação ao servidor. Por favor, certifique-se que o servidor está a correr e tente de novo.",
        ),
        button_got_it: Some("Entendido"),
    },
    Entry {
        lang: "ro",
        header_connect_to_server: Some("Conectați-vă la server"),
        label_server_host: Some("Gazdă"),
        label_server_host_help: Some("192.168.1.100:8096 sau https://myserver.com"),
        connect: Some("Conectare"),
        header_connection_failure: Some("Conexiune eșuată"),
        message_unable_to_connect_to_server: Some(
            "Nu putem să ne conectăm la serverul selectat în acest moment. Vă rugăm să vă asigurați că funcționează și încercați din nou.",
        ),
        button_got_it: Some("Am înțeles"),
    },
    Entry {
        lang: "ru",
        header_connect_to_server: Some("Соединение с сервером"),
        label_server_host: Some("Узел"),
        label_server_host_help: Some("192.168.1.100:8096 или https://myserver.com"),
        connect: Some("Соединиться"),
        header_connection_failure: Some("Сбой соединения"),
        message_unable_to_connect_to_server: Some(
            "Мы не можем подсоединиться к выбранному серверу в данный момент. Убедитесь, что он запущен и повторите попытку.",
        ),
        button_got_it: Some("Понятно"),
    },
    Entry {
        lang: "si",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: None,
    },
    Entry {
        lang: "sk",
        header_connect_to_server: Some("Pripojiť sa k serveru"),
        label_server_host: Some("Hosť"),
        label_server_host_help: Some("192.168.1.100:8096 alebo https://mojserver.sk"),
        connect: Some("Pripojiť"),
        header_connection_failure: Some("Pripojenie zlyhalo"),
        message_unable_to_connect_to_server: Some(
            "Nie sme schopný sa aktuálne pripojiť k vybranému serveru. Prosím, uistite sa, že je spustený a skúste to znovu.",
        ),
        button_got_it: Some("Rozumiem"),
    },
    Entry {
        lang: "sl-si",
        header_connect_to_server: Some("Poveži s strežnikom"),
        label_server_host: Some("Naslov strežnika"),
        label_server_host_help: Some("192.168.1.100:8096 ali https://myserver.com"),
        connect: Some("Poveži"),
        header_connection_failure: Some("Napaka povezave"),
        message_unable_to_connect_to_server: Some(
            "Povezava s strežnikom trenutno ni mogoča. Preverite, da je strežnik zagnan in poskusite ponovno.",
        ),
        button_got_it: Some("Razumem"),
    },
    Entry {
        lang: "so",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: None,
    },
    Entry {
        lang: "sq",
        header_connect_to_server: Some("Lidhuni me serverin"),
        label_server_host: None,
        label_server_host_help: None,
        connect: Some("Lidhu"),
        header_connection_failure: Some("Dështim në lidhje"),
        message_unable_to_connect_to_server: None,
        button_got_it: Some("Kuptova"),
    },
    Entry {
        lang: "sr",
        header_connect_to_server: Some("Повежи се са сервером"),
        label_server_host: Some("Домаћин"),
        label_server_host_help: Some("192.168.1.100:8096 или https://myserver.com"),
        connect: Some("Повежи"),
        header_connection_failure: Some("Спајање неуспешно"),
        message_unable_to_connect_to_server: Some(
            "Тренутно нисмо у могућности да се повежемо са изабраним сервером. Уверите се да је покренут и покушајте поново.",
        ),
        button_got_it: Some("У реду"),
    },
    Entry {
        lang: "sv",
        header_connect_to_server: Some("Anslut till server"),
        label_server_host: Some("Värd"),
        label_server_host_help: Some("192.168.1.100:8096 eller https://min.server.com"),
        connect: Some("Anslut"),
        header_connection_failure: Some("Misslyckad anslutning"),
        message_unable_to_connect_to_server: Some(
            "Vi kunde inte upprätta en anslutning till vald server just nu. Försäkra dig om att den är påslagen och försök igen.",
        ),
        button_got_it: Some("Ok"),
    },
    Entry {
        lang: "ta",
        header_connect_to_server: Some("சேவையகத்துடன் இணைக்கவும்"),
        label_server_host: Some("தொகுப்பாளர்"),
        label_server_host_help: Some("192.168.1.100:8096 or https://myserver.com"),
        connect: Some("இணைக்கவும்"),
        header_connection_failure: Some("இணைப்பு தோல்வி"),
        message_unable_to_connect_to_server: Some(
            "தேர்ந்தெடுக்கப்பட்ட சேவையகத்துடன் இப்போது எங்களால் இணைக்க முடியவில்லை. இது இயங்குவதை உறுதிசெய்து மீண்டும் முயற்சிக்கவும்.",
        ),
        button_got_it: Some("அறிந்துகொண்டேன்"),
    },
    Entry {
        lang: "te",
        header_connect_to_server: Some("సర్వర్‌కు కనెక్ట్ అవ్వండి"),
        label_server_host: Some("హోస్ట్"),
        label_server_host_help: Some("192.168.1.100:8096 లేదా https://myserver.com"),
        connect: Some("కనెక్ట్ చేయండి"),
        header_connection_failure: Some("కనెక్షన్ వైఫల్యం"),
        message_unable_to_connect_to_server: Some(
            "మేము ప్రస్తుతం ఎంచుకున్న సర్వర్‌కు కనెక్ట్ చేయలేకపోయాము. దయచేసి ఇది నడుస్తున్నట్లు నిర్ధారించుకోండి మరియు మళ్లీ ప్రయత్నించండి.",
        ),
        button_got_it: Some("దొరికింది"),
    },
    Entry {
        lang: "th",
        header_connect_to_server: Some("เชื่อมต่อเซิฟเวอร์"),
        label_server_host: None,
        label_server_host_help: None,
        connect: Some("เชื่อมต่อ"),
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: None,
    },
    Entry {
        lang: "tr",
        header_connect_to_server: Some("Sunucuya Bağlan"),
        label_server_host: Some("Ana Bilgisayar"),
        label_server_host_help: Some("192.168.1.100:8096 veya https://sunucum.com"),
        connect: Some("Bağlan"),
        header_connection_failure: Some("Bağlantı Hatası"),
        message_unable_to_connect_to_server: Some(
            "Seçilen sunucuya şu anda bağlanamıyoruz. Lütfen sunucunun çalıştığından emin olun ve tekrar deneyin.",
        ),
        button_got_it: Some("Anlaşıldı"),
    },
    Entry {
        lang: "ug",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: None,
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: None,
    },
    Entry {
        lang: "uk",
        header_connect_to_server: Some("Підключення до сервера"),
        label_server_host: Some("Хост"),
        label_server_host_help: Some("192.168.1.100:8096 або https://myserver.com"),
        connect: Some("Підключитись"),
        header_connection_failure: Some("Помилка підключення"),
        message_unable_to_connect_to_server: Some(
            "Наразі неможливо підключитися до обраного сервера. Будь ласка, переконайтеся, що він запущений і спробуйте ще раз.",
        ),
        button_got_it: Some("Зрозуміло"),
    },
    Entry {
        lang: "ur_PK",
        header_connect_to_server: Some("سرور سے جڑیں"),
        label_server_host: Some("میزبان"),
        label_server_host_help: Some("192.168.1.100:8096 یا https://myserver.com"),
        connect: Some("جڑیں"),
        header_connection_failure: Some("کنکشن کی ناکامی"),
        message_unable_to_connect_to_server: Some(
            "ہم ابھی منتخب سرور سے رابطہ قائم کرنے سے قاصر ہیں۔ براہ کرم یقینی بنائیں کہ یہ چل رہا ہے اور دوبارہ کوشش کریں۔",
        ),
        button_got_it: Some("یہ مل گیا"),
    },
    Entry {
        lang: "uz",
        header_connect_to_server: Some("Serverga ulanish"),
        label_server_host: None,
        label_server_host_help: None,
        connect: Some("Ulanish"),
        header_connection_failure: Some("Ulanish muvaffaqiyatsiz tugadi"),
        message_unable_to_connect_to_server: None,
        button_got_it: Some("Tushunarli"),
    },
    Entry {
        lang: "vi",
        header_connect_to_server: Some("Kết Nối Đến Máy Chủ"),
        label_server_host: Some("Máy chủ"),
        label_server_host_help: Some("192.168.1.100:8096 hoặc https://myserver.com"),
        connect: Some("Kết nối"),
        header_connection_failure: Some("Kế Nối Thất Bại"),
        message_unable_to_connect_to_server: Some(
            "Chúng tôi không thể kết nối với máy chủ đã chọn ngay bây giờ. Hãy đảm bảo rằng nó đang chạy và thử lại.",
        ),
        button_got_it: Some("Hiểu rồi"),
    },
    Entry {
        lang: "zh-cn",
        header_connect_to_server: Some("连接到服务器"),
        label_server_host: Some("主机"),
        label_server_host_help: Some("192.168.1.100:8096 或 https://myserver.com"),
        connect: Some("连接"),
        header_connection_failure: Some("连接失败"),
        message_unable_to_connect_to_server: Some(
            "现在无法连接所选择的服务器，请确保该服务器目前正在运行。",
        ),
        button_got_it: Some("知道了"),
    },
    Entry {
        lang: "zh-hk",
        header_connect_to_server: Some("連接至伺服器"),
        label_server_host: Some("主機"),
        label_server_host_help: Some("192.168.1.100:8096 或是 https://myserver.com"),
        connect: Some("連接"),
        header_connection_failure: Some("連接失敗"),
        message_unable_to_connect_to_server: Some(
            "無法連接到所選的伺服器，請先檢查伺服器的運作情況。",
        ),
        button_got_it: Some("了解"),
    },
    Entry {
        lang: "zh-tw",
        header_connect_to_server: Some("連接至伺服器"),
        label_server_host: Some("主機"),
        label_server_host_help: Some("192.168.1.100:8096 或是 https://myserver.com"),
        connect: Some("連線"),
        header_connection_failure: Some("連接失敗"),
        message_unable_to_connect_to_server: Some("無法連上所選的伺服器，請確保伺服器正在運作中。"),
        button_got_it: Some("我知道了"),
    },
    Entry {
        lang: "zu",
        header_connect_to_server: None,
        label_server_host: None,
        label_server_host_help: None,
        connect: Some("Xhuma"),
        header_connection_failure: None,
        message_unable_to_connect_to_server: None,
        button_got_it: None,
    },
];

fn find(lang: &str) -> Option<&'static Entry> {
    LANGUAGES.iter().find(|e| e.lang == lang)
}

#[allow(clippy::expect_used)] // table invariant: the fallback language is present
fn fallback() -> &'static Entry {
    find(FALLBACK_LANGUAGE).expect("fallback language missing from LANGUAGES")
}

/// Exact tag, then the primary subtag, then [`FALLBACK_LANGUAGE`].
pub fn entry_for(locale: &str) -> &'static Entry {
    find(locale)
        .or_else(|| find(locale.split('-').next().unwrap_or(locale)))
        .unwrap_or_else(fallback)
}

/// Per-field fallback to [`FALLBACK_LANGUAGE`], with "Server Address" as the
/// last resort for the host label.
pub fn strings_for(locale: &str) -> Strings {
    let e = entry_for(locale);
    let f = fallback();
    let edit = edit_entry_for(locale);
    Strings {
        server_host: e
            .label_server_host
            .or(f.label_server_host)
            .unwrap_or("Server Address"),
        server_host_help: e
            .label_server_host_help
            .or(f.label_server_host_help)
            .unwrap_or_default(),
        connect: e.connect.or(f.connect).unwrap_or_default(),
        connection_failure: e
            .header_connection_failure
            .or(f.header_connection_failure)
            .unwrap_or_default(),
        unable_to_connect: e
            .message_unable_to_connect_to_server
            .or(f.message_unable_to_connect_to_server)
            .unwrap_or_default(),
        got_it: e.button_got_it.or(f.button_got_it).unwrap_or_default(),
        undo: edit.undo,
        redo: edit.redo,
        cut: edit.cut,
        copy: edit.copy,
        paste: edit.paste,
        select_all: edit.select_all,
    }
}

/// Exact tag, then the primary subtag, then [`FALLBACK_LANGUAGE`]; the last is
/// the only entry the table is guaranteed to hold, so a miss on it yields the
/// first entry rather than none.
fn edit_entry_for(locale: &str) -> &'static EditEntry {
    let find = |lang: &str| EDIT_LANGUAGES.iter().find(|e| e.lang == lang);
    find(locale)
        .or_else(|| find(locale.split('-').next().unwrap_or(locale)))
        .or_else(|| find(FALLBACK_LANGUAGE))
        .unwrap_or(&EDIT_LANGUAGES[0])
}

/// The overlay's strings, resolved once from the system locale.
pub fn strings() -> &'static Strings {
    static STRINGS: std::sync::OnceLock<Strings> = std::sync::OnceLock::new();
    STRINGS.get_or_init(|| strings_for(&system_locale()))
}

/// `LC_ALL`, `LC_MESSAGES`, `LANG`, lowercased, `_` folded to `-`, encoding and
/// modifier suffixes stripped; [`FALLBACK_LANGUAGE`] when none is set.
pub fn system_locale() -> String {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        let Ok(raw) = std::env::var(key) else {
            continue;
        };
        let trimmed = raw.split('.').next().unwrap_or(&raw);
        let trimmed = trimmed.split('@').next().unwrap_or(trimmed);
        if trimmed.is_empty() || trimmed == "C" || trimmed == "POSIX" {
            continue;
        }
        return trimmed.to_lowercase().replace('_', "-");
    }
    FALLBACK_LANGUAGE.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_is_present() {
        assert_eq!(LANGUAGES.len(), 93);
        assert!(find("af").is_some());
        assert!(find("zu").is_some());
        assert!(find("bn_BD").is_some());
    }

    #[test]
    fn exact_tag_wins_over_primary_subtag() {
        assert_eq!(entry_for("pt-br").lang, "pt-br");
        assert_eq!(entry_for("de-ch").lang, "de");
        assert_eq!(entry_for("xx-yy").lang, FALLBACK_LANGUAGE);
    }

    #[test]
    fn missing_host_label_falls_back_per_field() {
        let af = strings_for("af");
        assert_eq!(af.connect, "Konnekteer");
        assert_eq!(af.server_host, strings_for(FALLBACK_LANGUAGE).server_host);
    }
}
