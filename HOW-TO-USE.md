# Guide d'utilisation d'addok-rs

addok-rs géocode des adresses françaises, par lots, à partir de la Base Adresse Nationale (BAN). Ce guide explique comment l'installer, construire son index, géocoder des fichiers, faire tourner le serveur et le mettre en production. Pour savoir ce qu'est addok-rs et ce qu'il change par rapport à addok, voir le [README](README.md).

## Sommaire

1. [Choisir son mode d'utilisation](#1-choisir-son-mode-dutilisation)
2. [Installer](#2-installer)
3. [Construire l'index](#3-construire-lindex)
4. [Géocoder un fichier en local : `addok-cli batch`](#4-géocoder-un-fichier-en-local--addok-cli-batch)
5. [Faire tourner le serveur : `addok-cli serve`](#5-faire-tourner-le-serveur--addok-cli-serve)
6. [Référence de l'API HTTP](#6-référence-de-lapi-http)
7. [Exemples de clients](#7-exemples-de-clients)
8. [Lire les résultats](#8-lire-les-résultats)
9. [Mettre à jour la BAN](#9-mettre-à-jour-la-ban)
10. [Mettre en production](#10-mettre-en-production)
11. [Passer d'addok à addok-rs](#11-passer-daddok-à-addok-rs)
12. [Dépannage](#12-dépannage)
13. [Limites actuelles](#13-limites-actuelles)

---

## 1. Choisir son mode d'utilisation

addok-rs est un seul programme, `addok-cli`, avec trois commandes :

| Commande | Rôle |
|---|---|
| `addok-cli build` | Construit l'index à partir de l'export de la BAN : 85 s, un fichier de 1,86 Go. |
| `addok-cli batch` | Géocode un fichier entier, Parquet ou CSV, sur tous les cœurs, sans serveur. |
| `addok-cli serve` | Sert l'index en HTTP, sur le port 7878 comme addok. |

Quel que soit le mode, il faut d'abord [construire l'index](#3-construire-lindex). Ensuite :

| Besoin | Solution |
|---|---|
| Géocoder un fichier sur la machine où il se trouve | [`addok-cli batch`](#4-géocoder-un-fichier-en-local--addok-cli-batch) : ni réseau, ni découpage à gérer |
| Offrir le géocodage à d'autres programmes ou d'autres machines | [`addok-cli serve`](#5-faire-tourner-le-serveur--addok-cli-serve), avec [`/batch`](#post-batch) |
| Remplacer un addok existant sans toucher à ses clients | [`addok-cli serve`](#5-faire-tourner-le-serveur--addok-cli-serve), avec [`/search/csv`](#post-searchcsv) (voir [Passer d'addok à addok-rs](#11-passer-daddok-à-addok-rs)) |

---

## 2. Installer

### Avec l'image Docker (recommandé)

L'image est publiée sur le GitHub Container Registry pour **linux/amd64** et **linux/arm64**. Elle ne contient que le binaire, sur Debian, et tourne sous un utilisateur système sans privilèges.

```sh
docker pull ghcr.io/wildbenji/addok-rs
```

- **`latest`** désigne la dernière version publiée.
- **`X.Y.Z`** désigne une version précise, et **`X.Y`** la dernière version corrective d'une version mineure. En production, épinglez une version : l'index d'une version plus ancienne peut devoir être reconstruit (voir [Après une mise à jour d'addok-rs](#après-une-mise-à-jour-daddok-rs)).

Par défaut, l'image lance `addok-cli serve /data/ban.addok`. Pour les autres commandes, il faut remplacer le point d'entrée : `--entrypoint addok-cli`.

### Depuis les sources

Il faut **Rust stable**, installé avec [rustup](https://rustup.rs). Le fichier `rust-toolchain.toml` du dépôt sélectionne la bonne chaîne d'outils tout seul.

```sh
git clone https://github.com/WildBenji/addok-rs.git
cd addok-rs
cargo build --release
```

Le binaire se trouve dans `target/release/addok-cli`. Il est autonome : on peut le copier où l'on veut, avec le fichier d'index. Lancé sans argument, il affiche toutes ses options ; `addok-cli --version` donne sa version.

Dans la suite, `addok-cli` désigne ce binaire.

### Ressources nécessaires

| | Construction de l'index | Géocodage |
|---|---|---|
| **Mémoire** | environ 3 Go (pic mesuré : 2,7 Go) | de quoi garder en cache l'essentiel de l'index (1,86 Go), en plus du reste |
| **Disque** | 1,3 Go pour l'export de la BAN, 1,9 Go pour l'index | 1,9 Go pour l'index |
| **Cœurs** | un seul | tous ceux disponibles, ou `--cores N` |

Prévoyez le double de place pour l'index pendant une mise à jour : l'ancien reste en place jusqu'à la fin de la construction du nouveau.

---

## 3. Construire l'index

L'index est un fichier unique qu'addok-rs construit à partir de l'**export Addok de la BAN**. La BAN le republie chaque nuit sur data.gouv.fr. Il couvre la France entière, outre-mer compris.

### Télécharger l'export

```sh
curl -LO https://adresse.data.gouv.fr/data/ban/adresses/latest/addok/adresses-addok-france.ndjson.gz
```

Le fichier fait environ 1,3 Go. Les éditions précédentes sont dans le [répertoire Addok de la BAN](https://adresse.data.gouv.fr/data/ban/adresses/latest/addok).

> ℹ️ L'archive « prête à l'emploi » que la BAN publie pour addok (`addok.db`, `dump.rdb`) ne sert pas : addok-rs construit son propre index à partir de l'export NDJSON.

### Lancer la construction

```sh
addok-cli build adresses-addok-france.ndjson.gz ban.addok
```

Avec Docker, depuis le dossier qui contient l'export :

```sh
docker run --rm --user "$(id -u):$(id -g)" -v "$PWD:/data" --entrypoint addok-cli \
  ghcr.io/wildbenji/addok-rs build /data/adresses-addok-france.ndjson.gz /data/ban.addok
```

L'option `--user` fait écrire l'index sous votre utilisateur plutôt que sous celui de l'image. La construction affiche :

```
ban.addok: 1.86 GB in 41 sections, written in 85s
```

- **L'export peut être compressé ou non :** `build` lit le `.ndjson.gz` tel quel, ou un `.ndjson` décompressé.
- **La construction utilise un seul cœur** et dure environ une minute et demie.
- **L'index en place n'est jamais abîmé.** Le nouveau est d'abord écrit à côté, dans `ban.addok.partial`, puis renommé d'un coup sur `ban.addok`. Si la construction échoue (ligne illisible, disque plein), l'ancien index reste intact et le fichier partiel est supprimé.
- **Un serveur en cours d'exécution n'est pas perturbé** par une reconstruction de son propre fichier : il garde l'ancien index jusqu'à son redémarrage (voir [Mettre à jour la BAN](#9-mettre-à-jour-la-ban)).

### Après une mise à jour d'addok-rs

Le format de l'index peut gagner des sections d'une version à l'autre. Un index construit par une version plus ancienne est alors refusé avec un message explicite :

```
ban.addok: index without a WordTable section: written by an older addok-rs, or damaged; rebuild it with `addok-cli build`
```

Il suffit de relancer `addok-cli build`.

---

## 4. Géocoder un fichier en local : `addok-cli batch`

C'est la façon la plus simple et la plus rapide de géocoder un fichier : pas de serveur, pas de réseau, tous les cœurs de la machine.

```sh
addok-cli batch ban.addok adresses.parquet geocodees.parquet --columns adresse,ville,code_postal
```

```
ban.addok: 2464369 documents, opened in 112.2µs
geocodees.parquet: 100000 rows geocoded in 2.2 s, 44745 rows/s
```

Avec Docker, depuis le dossier qui contient l'index et le fichier :

```sh
docker run --rm --user "$(id -u):$(id -g)" -v "$PWD:/data" --entrypoint addok-cli \
  ghcr.io/wildbenji/addok-rs batch /data/ban.addok /data/adresses.parquet /data/geocodees.parquet \
  --columns adresse,ville,code_postal
```

Le fichier de sortie reprend **toutes les lignes et toutes les colonnes** du fichier d'entrée, dans le même ordre, et y ajoute les [colonnes de résultat](#8-lire-les-résultats).

### Options

| Option | Rôle | Par défaut |
|---|---|---|
| `--columns A,B,C` | Les colonnes qui forment la requête, jointes dans cet ordre avec une espace | toutes les colonnes |
| `--input-format FORMAT` | `parquet` ou `csv` | d'après l'extension du fichier d'entrée |
| `--output-format FORMAT` | `parquet` ou `csv` | d'après l'extension du fichier de sortie |
| `--input-delimiter C` | Séparateur du CSV d'entrée, un caractère | `;` |
| `--output-delimiter C` | Séparateur du CSV de sortie, un caractère | `;` |
| `--min-score X` | Score arrondi qu'un résultat doit dépasser pour être retenu | `0.5` |
| `--postcode-fallback on` | Le [repli sur la commune](#code-postal-faux--le-repli-sur-la-commune) quand le code postal est faux | désactivé |
| `--result-columns A,B` | Ajoute le [numéro découpé](#le-numéro-découpé--numéro-complément-forme-courte) : `result_num`, `result_num_complement`, `result_num_complement_short` | aucune |
| `--filters F=COL,F=COL` | [Filtre](#filtrer-les-résultats) chaque ligne par la valeur de la colonne `COL`, pour chaque filtre `F` : `type`, `citycode` ou `postcode` | aucun |
| `--cores N` | Nombre de cœurs à utiliser, de 1 au nombre disponible | tous |

Les extensions reconnues sont `.parquet` et `.pq` pour Parquet, `.csv` et `.txt` pour CSV. Pour tout autre nom, il faut préciser `--input-format` ou `--output-format`.

### Exemples

```sh
# Parquet vers Parquet : le cas courant
addok-cli batch ban.addok adresses.parquet geocodees.parquet --columns adresse,ville,code_postal

# CSV séparé par des points-virgules vers Parquet
addok-cli batch ban.addok adresses.csv geocodees.parquet --columns adresse,ville,cp

# Parquet vers CSV séparé par des virgules
addok-cli batch ban.addok adresses.parquet geocodees.csv --columns adresse,ville,code_postal --output-delimiter ,

# Sur 4 cœurs seulement, pour laisser de la place au reste de la machine
addok-cli batch ban.addok adresses.parquet geocodees.parquet --columns adresse,ville,code_postal --cores 4
```

### Choisir les colonnes de la requête

La requête d'une ligne est la **jointure, avec une espace, des colonnes données à `--columns`**, dans leur ordre. Une valeur vide ou nulle compte pour une chaîne vide. C'est exactement ce que fait addok-csv.

- **L'ordre recommandé est adresse, ville, code postal.** C'est celui sur lequel les réponses ont été comparées à celles d'addok.
- **Une ville ou un code postal manquant n'empêche pas le géocodage.** L'adresse est complétée à partir de la BAN (voir [Lire les résultats](#compléter-les-adresses-incomplètes)).
- **Les noms de colonnes ne peuvent pas contenir de virgule** avec `--columns`. L'interface HTTP n'a pas cette limite, chaque colonne y étant un champ distinct.
- **Les colonnes Parquet non textuelles** (un code postal stocké en entier, par exemple) sont converties en texte. Un code postal stocké en entier a cependant perdu son zéro initial (`1400` au lieu de `01400`) ; mieux vaut le garder en texte.

### Formats

- **Parquet :** les colonnes d'entrée gardent leur type (un identifiant entier reste entier), et le fichier de sortie est compressé en zstd.
- **CSV en entrée :** UTF-8, avec ou sans marque d'ordre d'octets (BOM). La première ligne donne les noms de colonnes. Les champs entre guillemets peuvent contenir le séparateur ou des sauts de ligne. Toutes les colonnes sont lues comme du texte ; le CSV ne sait pas distinguer une valeur vide d'une valeur nulle.
- **CSV en sortie :** UTF-8 sans BOM, une ligne d'en-tête, guillemets seulement quand c'est nécessaire, fins de ligne `\n`.

---

## 5. Faire tourner le serveur : `addok-cli serve`

```sh
addok-cli serve ban.addok --host 0.0.0.0 --port 7878
```

```
ban.addok: 2464369 documents, opened in 42.0µs
Serving HTTP on 0.0.0.0:7878, on 8 cores…
```

| Option | Rôle | Par défaut |
|---|---|---|
| `--host HOST` | Adresse d'écoute (`0.0.0.0` pour être joignable depuis d'autres machines ou conteneurs) | `127.0.0.1` |
| `--port PORT` | Port d'écoute | `7878`, comme addok |
| `--cores N` | Nombre de cœurs pour géocoder, de 1 au nombre disponible | tous |

- **Le démarrage est instantané :** l'index est projeté en mémoire, pas chargé. Les premières requêtes sont un peu plus lentes, le temps que le système mette l'index en cache.
- **L'arrêt se fait avec `Ctrl-C`.** Le serveur finit les requêtes en cours avant de s'arrêter.
- **Chaque requête est traitée sur un cœur,** ses lignes dans l'ordre, et au plus `--cores` requêtes sont traitées en même temps ; les suivantes attendent leur tour. Pour occuper tous les cœurs, un client doit donc envoyer plusieurs requêtes à la fois (voir [Exemples de clients](#7-exemples-de-clients)).
- **Dans un conteneur limité à quelques CPU,** addok-rs voit cette limite : c'est elle qui sert de défaut et de maximum pour `--cores`.

### Avec Docker

Le dossier monté sur `/data` doit contenir l'index sous le nom `ban.addok`. La lecture seule (`:ro`) suffit.

```sh
docker run -d --name addok-rs --restart unless-stopped \
  -p 7878:7878 -v /chemin/vers/le/dossier:/data:ro \
  ghcr.io/wildbenji/addok-rs
```

Avec docker compose :

```yaml
services:
  addok-rs:
    image: ghcr.io/wildbenji/addok-rs:latest   # en production, une version précise
    ports:
      - "7878:7878"
    volumes:
      - ./index:/data:ro
    restart: unless-stopped
    # command: ["--cores", "4"]   # par défaut : tous les cœurs du conteneur
```

- **Les cœurs :** par défaut, tous ceux que le conteneur peut utiliser, limite `--cpus` comprise. Tout argument placé après le nom de l'image s'ajoute à `serve`, donc `docker run … ghcr.io/wildbenji/addok-rs --cores 4` en prend 4.
- **Un autre nom d'index :** remplacer la commande, par exemple `docker run … --entrypoint addok-cli ghcr.io/wildbenji/addok-rs serve /data/autre.addok --host 0.0.0.0`.
- **La mémoire :** l'image n'en fixe aucune limite. L'index (1,86 Go) est projeté en mémoire, et le système garde en cache ce qu'il en lit, jusqu'à environ 1,9 Go en charge. Si le conteneur a une limite mémoire, prévoyez de quoi garder l'index en cache en plus du reste : sous la limite, le système l'évince, et chaque recherche attend alors le disque.
- **L'arrêt :** `docker stop` arrête le serveur proprement, après les requêtes en cours.
- **Pas de `HEALTHCHECK` dans l'image,** qui n'a pas curl : interrogez [`/health`](#get-health) depuis l'extérieur.
- **Construire l'image soi-même :** `docker build -t addok-rs .` à la racine du dépôt, sans BuildKit ni buildx, pour l'architecture de la machine.

---

## 6. Référence de l'API HTTP

| Point d'entrée | Rôle |
|---|---|
| [`POST /batch`](#post-batch) | Géocode un fichier Parquet ou CSV et le rend en Parquet ou CSV. L'interface à privilégier. |
| [`POST /search/csv`](#post-searchcsv) | Le point d'entrée d'addok-csv, à l'octet près, pour les clients d'addok existants. |
| [`GET /health`](#get-health) | Dit si le serveur répond, avec l'index chargé et la version. |

Les deux points d'entrée de géocodage reçoivent un formulaire `multipart/form-data` : le fichier dans le champ `data`, et chaque paramètre dans son propre champ. Seuls les champs du formulaire comptent : les paramètres passés dans l'URL sont ignorés, comme dans addok-csv.

### `POST /batch`

`POST /batch` fait la même chose que [`addok-cli batch`](#4-géocoder-un-fichier-en-local--addok-cli-batch), à travers HTTP.

| Champ | Rôle | Par défaut |
|---|---|---|
| `data` | Le fichier à géocoder (obligatoire) | — |
| `columns` | Une colonne de la requête ; répéter le champ pour chaque colonne, dans l'ordre | toutes les colonnes |
| `input_format` | `parquet` ou `csv` | d'après l'extension du nom de fichier envoyé |
| `output_format` | `parquet` ou `csv` | le format d'entrée |
| `input_delimiter` | Séparateur du CSV d'entrée | `;` |
| `output_delimiter` | Séparateur du CSV de sortie | `;` |
| `min_score` | Score arrondi qu'un résultat doit dépasser | `0.5` |
| `postcode_fallback` | `1` pour le [repli sur la commune](#code-postal-faux--le-repli-sur-la-commune) (`true`, `on` et `yes` valent aussi) | désactivé |
| `result_columns` | Une colonne de résultat en plus ; répéter le champ pour chacune : `result_num`, `result_num_complement`, `result_num_complement_short` ([numéro découpé](#le-numéro-découpé--numéro-complément-forme-courte)). Les autres noms sont ignorés | aucune |
| `type`, `citycode`, `postcode` | Le nom d'une colonne dont la valeur [filtre](#filtrer-les-résultats) chaque ligne | aucun filtre |

**La réponse** est le fichier géocodé, en `application/vnd.apache.parquet` ou en `text/csv; charset=utf-8`, proposé sous le nom `<nom-envoyé>.geocoded.parquet` (ou `.csv`).

- **Une requête trop longue ne fait pas échouer le fichier.** Au-delà de 200 caractères, elle est refusée par le moteur, comme dans addok, mais seule sa ligne reste sans résultat. La réponse le signale par l'en-tête [`X-Addok-Warning`](#len-tête-x-addok-warning).
- **Les résultats prennent la place des colonnes homonymes.** Si le fichier d'entrée contient déjà une colonne portant le nom d'une colonne de résultat (`result_label`, par exemple), elle est remplacée.

### `POST /search/csv`

`POST /search/csv` reproduit **à l'octet près** le point d'entrée d'addok-csv 1.1.0, trois défauts en moins ([ci-dessous](#trois-défauts-daddok-csv-corrigés)). Il existe pour que les clients d'addok passent à addok-rs en changeant seulement l'URL. Pour un nouveau client, préférez [`/batch`](#post-batch).

| Champ | Rôle | Par défaut |
|---|---|---|
| `data` | Le fichier CSV (obligatoire) | — |
| `columns` | Une colonne de la requête ; répéter le champ pour chaque colonne | toutes les colonnes |
| `delimiter` | Séparateur du CSV, un caractère | détecté automatiquement |
| `quote` | Caractère de guillemet | détecté automatiquement |
| `encoding` | `utf-8` ou `utf-8-sig` | `utf-8-sig` |
| `min_score` | Score arrondi qu'un résultat doit dépasser | `0.5` |
| `with_bom` | `true` pour ajouter une marque d'ordre d'octets en tête de réponse | `false` |
| `postcode_fallback` | `1` pour le [repli sur la commune](#code-postal-faux--le-repli-sur-la-commune) (propre à addok-rs) | désactivé |
| `result_columns` | Comme dans `/batch` : seuls `result_num`, `result_num_complement` et `result_num_complement_short` ajoutent une colonne, après les 16 d'addok-csv (propre à addok-rs). Les autres noms ne changent rien, comme dans addok-csv | aucune |
| `type`, `citycode`, `postcode` | Le nom d'une colonne dont la valeur [filtre](#filtrer-les-résultats) chaque ligne, comme dans addok-csv | aucun filtre |

**La réponse :**

- **Le fichier d'entrée, colonnes et lignes dans le même ordre,** suivi des 16 colonnes de résultat (voir [Lire les résultats](#8-lire-les-résultats)).
- **Tout est en texte :** une ligne sans résultat a des cellules de résultat vides.
- **Le format reste celui d'addok-csv :** le même séparateur et le même guillemet que l'entrée, des fins de ligne `\r\n`, et une marque d'ordre d'octets en tête (l'encodage `utf-8-sig` par défaut l'ajoute toujours). Les nombres sont écrits comme Python les écrit (`0.9`, `46.124404`). Si aucun séparateur n'a pu être deviné, la réponse met chaque champ entre guillemets et finit ses lignes par `\n`, comme addok-csv.

#### Trois défauts d'addok-csv, corrigés

`/search/csv` reproduit addok-csv en tout, sauf trois défauts de son transport, corrigés plutôt que reproduits :

- **Le séparateur mal deviné.** addok-csv devine le séparateur en analysant tout le fichier, et se trompe parfois : une adresse comme `2 RUE L 'EST ' B` lui fait prendre l'espace pour séparateur et l'apostrophe pour guillemet. Il refuse alors la requête avec une erreur 400 (`Cannot found column 'adresse' in columns ['adresse,ville,code_postal']`), ce qui arrive à environ **une requête sur cent** sur des données réelles. addok-rs devine comme lui, mais quand l'en-tête ainsi lu ne contient pas les colonnes demandées, il relit le fichier avec les séparateurs usuels (`,`, `;`, tabulation, `|`) et le guillemet `"`, et garde le premier qui les trouve. Partout où addok-csv lisait juste, rien ne change.
- **La colonne `result_street`,** qu'addok-csv écrit toujours vide (aucun document de la BAN ne porte de rue), n'est plus écrite.
- **Une ligne trop longue ne fait plus échouer toute la requête.** addok refuse une requête de plus de 200 caractères, et addok-csv refuse alors le fichier entier (erreur 413) : une seule ligne de texte parasite coûtait leur résultat à toutes les autres. Désormais, cette ligne reste sans résultat, les autres sont géocodées, et l'en-tête [`X-Addok-Warning`](#len-tête-x-addok-warning) la signale.

#### Les filtres, qu'addok-csv 1.1.0 n'applique plus

Avec addok 1.3.2, addok-csv 1.1.0 échoue dès qu'on lui passe un filtre. addok-rs applique les filtres comme addok-csv l'entend : chaque champ nomme une colonne, dont la valeur filtre sa ligne (voir [Filtrer les résultats](#filtrer-les-résultats)).

#### Paramètres refusés

Le géocodage autour d'un point (`lat`, `lon`) n'est pas encore porté. Plutôt que de l'ignorer en silence, addok-rs refuse la requête avec une erreur 400 :

```json
{"title": "Unsupported parameter \"lat\""}
```

### `GET /health`

```sh
curl http://localhost:7878/health
```

```json
{"cores":8,"documents":2464369,"status":"HEALTHY","version":"0.8.0"}
```

- **La réponse est immédiate :** pas de géocodage, et pas d'attente derrière les requêtes en cours, même quand tous les cœurs sont occupés. C'est le point à interroger au démarrage, jusqu'à ce qu'il réponde, puis pour la supervision.
- **`documents`** est le nombre de documents de l'index chargé, de quoi vérifier que c'est le bon (2 464 369 pour l'export national du 2026-10-02).
- **`cores`** est le nombre de cœurs sur lesquels le serveur géocode, `--cores` ou limite du conteneur comprise.
- **`status`** vaut `HEALTHY`, comme le `/health` d'addok.
- **`version`** est celle d'`addok-cli`, que `addok-cli --version` donne aussi sans serveur.

### L'en-tête `X-Addok-Warning`

Quand une requête laisse des lignes sans résultat pour une autre raison qu'une adresse introuvable, la réponse le dit dans un en-tête, et le serveur l'écrit dans son journal :

```
X-Addok-Warning: query_too_long; limit=200; count=1; rows=352
```

- **`limit`** est la longueur maximale d'une requête, 200 caractères comme dans addok. La plus longue adresse réelle mesurée sur 9,5 millions de requêtes en fait 162.
- **`count`** est le nombre de lignes concernées, et **`rows`** leurs numéros, comptés à partir de 1 (les 100 premiers).
- **`addok-cli batch`** écrit le même avertissement sur la sortie d'erreur.

---

## 7. Exemples de clients

### curl

```sh
# Parquet vers Parquet, avec /batch
curl -F data=@adresses.parquet \
     -F columns=adresse -F columns=ville -F columns=code_postal \
     http://localhost:7878/batch -o geocodees.parquet

# CSV (séparé par des points-virgules) vers Parquet
curl -F data=@adresses.csv \
     -F columns=adresse -F columns=ville -F columns=code_postal \
     -F output_format=parquet \
     http://localhost:7878/batch -o geocodees.parquet

# CSV vers CSV, avec l'interface d'addok
curl -F data=@adresses.csv \
     -F columns=adresse -F columns=ville -F columns=code_postal \
     http://localhost:7878/search/csv -o geocodees.csv
```

> 💡 curl donne un sens spécial au `;` dans les valeurs de `-F`. Pour passer un point-virgule comme séparateur, utilisez `--form-string "input_delimiter=;"`.

### Python : un petit fichier

```python
import io

import polars as pl
import requests

adresses = pl.read_parquet("adresses.parquet")
tampon = io.BytesIO()
adresses.write_parquet(tampon)

reponse = requests.post(
    "http://localhost:7878/batch",
    files={"data": ("adresses.parquet", tampon.getvalue())},
    data={"columns": ["adresse", "ville", "code_postal"]},
    timeout=300,
)
reponse.raise_for_status()
geocodees = pl.read_parquet(io.BytesIO(reponse.content))
```

### Python : un gros fichier, à pleine vitesse

Une requête n'occupe qu'un cœur du serveur. Pour un gros fichier, découpez-le en **morceaux d'environ 1 000 lignes consécutives**, et gardez **au moins autant de requêtes en vol que le serveur a de cœurs** :

```python
import io
from concurrent.futures import ThreadPoolExecutor

import polars as pl
import requests

URL = "http://localhost:7878/batch"
COLONNES = ["adresse", "ville", "code_postal"]
LIGNES_PAR_REQUETE = 1_000
REQUETES_EN_VOL = 32  # au moins le nombre de cœurs du serveur


def geocoder(morceau: pl.DataFrame) -> pl.DataFrame:
    tampon = io.BytesIO()
    morceau.write_parquet(tampon)
    reponse = requests.post(
        URL,
        files={"data": ("morceau.parquet", tampon.getvalue())},
        data={"columns": COLONNES},
        timeout=300,
    )
    reponse.raise_for_status()
    return pl.read_parquet(io.BytesIO(reponse.content))


adresses = pl.read_parquet("adresses.parquet")
morceaux = [adresses.slice(debut, LIGNES_PAR_REQUETE)
            for debut in range(0, adresses.height, LIGNES_PAR_REQUETE)]
with ThreadPoolExecutor(REQUETES_EN_VOL) as pool:
    geocodees = pl.concat(list(pool.map(geocoder, morceaux)))  # map garde l'ordre
geocodees.write_parquet("geocodees.parquet")
```

Ainsi réglé, un client sur la même machine que le serveur dépasse 40 000 lignes/s sur 8 cœurs.

---

## 8. Lire les résultats

### Les colonnes de résultat

| Colonne | Contenu | Type Parquet |
|---|---|---|
| `latitude`, `longitude` | Position du résultat : celle du numéro quand un numéro a été trouvé, sinon celle de la rue, du lieu-dit ou de la commune | Float64 |
| `result_label` | Libellé complet, par exemple `131 Montée de Caluire 01400 Châtillon-sur-Chalaronne` | texte |
| `result_score` | Score du meilleur résultat, entre 0 et 1, arrondi à deux décimales | Float64 |
| `result_score_next` | Score du deuxième résultat, arrondi ; `0` s'il n'y en a pas | Float64 |
| `result_type` | `housenumber` (numéro), `street` (rue), `locality` (lieu-dit) ou `municipality` (commune) | texte |
| `result_id` | Identifiant BAN (clé d'interopérabilité) du numéro ou de la voie, par exemple `01093_0041_00131` ; code INSEE pour une commune | texte |
| `result_housenumber` | Numéro trouvé, tel que la BAN l'écrit (`131`, `4bis`…) ; vide sinon | texte |
| `result_name` | Nom de la voie, du lieu-dit ou de la commune, sans le numéro | texte |
| `result_postcode` | Code postal | texte |
| `result_city` | Commune | texte |
| `result_context` | Département, son nom et sa région, par exemple `01, Ain, Auvergne-Rhône-Alpes` | texte |
| `result_citycode` | Code INSEE de la commune | texte |
| `result_oldcitycode`, `result_oldcity` | Code INSEE et nom de l'ancienne commune, pour les communes fusionnées | texte |
| `result_district` | Arrondissement municipal (Paris, Lyon, Marseille) | texte |

En CSV, toutes ces valeurs sont du texte et une valeur absente est une cellule vide. En Parquet, une valeur absente est `null`.

### Le numéro découpé : numéro, complément, forme courte

`result_housenumber` donne le numéro tel que la BAN l'écrit, numéro et complément collés (`12bis`, `7C`). Sur demande, addok-rs le rend aussi découpé, en trois colonnes :

| Colonne | Contenu | `12bis` | `7C` | `11bis a` |
|---|---|---|---|---|
| `result_num` | Le numéro, ses seuls chiffres | `12` | `7` | `11` |
| `result_num_complement` | Le complément, tel que la BAN l'écrit (la référence) | `bis` | `C` | `bis a` |
| `result_num_complement_short` | Le complément en forme courte, en minuscules, pour rapprocher d'autres données : `bis` → `b`, `ter` → `t`, `quater` → `q`, `quinquies` → `c`, `sexies` → `s`, une lettre seule reste. Un complément d'une autre forme reste tel quel | `b` | `c` | `bis a` |

- **Pour les obtenir,** nommez les colonnes voulues dans `result_columns` (`-F result_columns=result_num -F result_columns=result_num_complement`), ou passez `--result-columns result_num,result_num_complement` à `addok-cli batch`. Sans cela, la réponse ne change pas. addok ignore ces noms : on peut les envoyer aux deux moteurs.
- **Elles sont vides** sans numéro, et les deux colonnes de complément sont vides sans complément.
- **La forme courte n'est jamais vide quand un complément existe :** une jointure sur cette colonne ne perd pas les compléments rares (`bis a`, `appt 1`), qui gardent leur forme.

### Le score et le seuil

- **Le score mesure la ressemblance** entre la requête et le libellé trouvé, pour l'essentiel, pondérée par l'importance de la voie ou de la commune. C'est l'échelle de scores d'addok, inchangée.
- **Une ligne n'a de résultat que si son meilleur score, arrondi à deux décimales, dépasse strictement `min_score`** (0,5 par défaut). Sinon, toutes ses colonnes de résultat restent vides (`null` en Parquet).
- **Un écart faible entre `result_score` et `result_score_next`** signale une ambiguïté : deux adresses ressemblent presque autant à la requête.
- **Le seuil de confiance au-delà duquel accepter un résultat** dépend de l'usage, et se calibre sur un échantillon vérifié de ses propres données. Un seuil réglé sur l'image `etalab/addok` est à recalibrer (voir [Passer d'addok à addok-rs](#11-passer-daddok-à-addok-rs)).

### Compléter les adresses incomplètes

Il n'y a rien de particulier à faire. Si le code postal ou la ville manque dans une ligne, laissez la cellule vide : la recherche s'appuie sur le reste de l'adresse. Le résultat apporte alors le code postal (`result_postcode`), la commune (`result_city`), le code INSEE (`result_citycode`) et le contexte (`result_context`) de l'adresse trouvée. Ce cas est vérifié face à addok, avec 99,90 % de réponses identiques sans code postal et 99,92 % sans ville.

### Filtrer les résultats

Les filtres d'addok ne gardent que les adresses d'un type, d'une commune ou d'un code postal :

| Filtre | Garde les résultats… |
|---|---|
| `type` | de ce type : `housenumber`, `street`, `locality` ou `municipality` |
| `citycode` | de cette commune (code INSEE) |
| `postcode` | de ce code postal |

Sur un fichier, un filtre ne prend pas une valeur mais **le nom d'une colonne** : chaque ligne est filtrée par sa propre valeur, comme dans addok-csv.

```sh
# Chaque adresse cherchée dans le code postal de sa colonne code_postal
curl -F data=@adresses.csv -F columns=adresse -F columns=ville \
     -F postcode=code_postal http://localhost:7878/search/csv -o geocodees.csv

# La même chose sans serveur, en ne gardant que les numéros
addok-cli batch ban.addok adresses.parquet geocodees.parquet \
     --columns adresse,ville --filters postcode=code_postal,type=type_voulu
```

- **Plusieurs filtres :** une adresse doit les satisfaire tous. **Plusieurs colonnes pour un même filtre** (`-F postcode=cp1 -F postcode=cp2`) : l'une ou l'autre de leurs valeurs suffit.
- **Une cellule vide ne filtre rien** pour sa ligne.
- **Une colonne absente du fichier est refusée,** comme une colonne de requête absente.
- **`type` décide du numéro.** Avec `housenumber` pour seule valeur, une ligne n'a de résultat que si son numéro est trouvé. Avec `street`, `locality` ou `municipality` seuls, le numéro de la requête est ignoré. Sans `type`, ou avec `housenumber` parmi d'autres valeurs, le numéro est cherché comme d'habitude.
- **Un filtre est une contrainte stricte.** Il réduit ce que la recherche peut trouver : sous filtre, addok ne tente pas non plus de corriger les fautes de frappe d'une adresse dont d'autres mots sont trouvés. addok-rs fait de même. Le [repli sur la commune](#code-postal-faux--le-repli-sur-la-commune) garde lui aussi les filtres : avec un filtre `postcode`, il ne peut pas sortir de ce code postal.

### Code postal faux : le repli sur la commune

Sur demande, un code postal faux ne fait plus perdre l'adresse. C'est le cas en particulier dans les villes à plusieurs codes postaux, quand le code saisi est celui de la ville mais d'un autre quartier. C'est la seule différence de géocodage voulue avec addok. Par exemple :

> `4 RUE CLEMENT MAROT, PERPIGNAN, 66100` rend `4 Rue Clément Marot 66000 Perpignan`.

- **Pour l'activer :** il est désactivé par défaut, et addok-rs répond alors comme addok.
  - Avec `/search/csv` ou `/batch`, ajoutez le champ `postcode_fallback=1` : `curl … -F postcode_fallback=1 …`. addok ignore ce champ, on peut donc l'envoyer aux deux moteurs.
  - Avec `addok-cli batch`, passez `--postcode-fallback on`.
  - Il coûte du débit : environ 35 000 lignes/s au lieu de 41 000, mesuré sur 648 328 adresses réelles.
- **Quand le repli s'applique :** le meilleur résultat d'une ligne n'est pas un numéro de score au moins 0,62, et la requête contient un code postal (un mot de cinq chiffres).
- **Quand il ne s'applique pas :** la première réponse est une voie, un lieu-dit ou une commune, située dans une commune que la requête nomme et qui n'a qu'un code postal. Le code postal est alors juste, et c'est le numéro qui manque à la BAN. Paris, Marseille et Lyon comptent pour toute la ville, tous arrondissements confondus.
- **Ce qu'il fait :** la ligne est cherchée une seconde fois, sans le code postal.
- **Quand la seconde réponse est gardée :** seulement si c'est un numéro de score au moins 0,62, dans une commune que la requête nomme (son nom actuel ou celui d'une commune fusionnée).
- **Sinon,** la première réponse reste.
- **Le score rendu** est alors celui de la requête sans code postal.
- **Ce qui ne change pas :** une réponse déjà sûre n'est jamais cherchée deux fois, et son score reste celui d'addok.

---

## 9. Mettre à jour la BAN

La BAN publie un nouvel export chaque nuit. Pour en profiter :

1. **Téléchargez** le nouvel `adresses-addok-france.ndjson.gz` ([section 3](#télécharger-lexport)).
2. **Reconstruisez l'index au même endroit :**
   ```sh
   addok-cli build adresses-addok-france.ndjson.gz ban.addok
   ```
   Le remplacement est atomique. Un serveur en cours d'exécution continue de répondre avec l'ancien index pendant toute la construction, et même après.
3. **Redémarrez le serveur** pour qu'il serve le nouvel index. Le redémarrage est quasi instantané, l'index n'ayant pas à être chargé.

Avec Docker, la construction se fait dans un conteneur à part, sur le même dossier, que le serveur monte en lecture seule :

```sh
cd /chemin/vers/le/dossier
curl -LO https://adresse.data.gouv.fr/data/ban/adresses/latest/addok/adresses-addok-france.ndjson.gz
docker run --rm --user "$(id -u):$(id -g)" -v "$PWD:/data" --entrypoint addok-cli \
  ghcr.io/wildbenji/addok-rs build /data/adresses-addok-france.ndjson.gz /data/ban.addok
docker restart addok-rs
curl http://localhost:7878/health   # « documents » donne le nombre du nouvel index
```

Avec `addok-cli batch`, il n'y a rien à redémarrer : chaque lancement ouvre l'index tel qu'il est.

---

## 10. Mettre en production

### Dimensionner

- **Les cœurs font le débit.** Comptez environ 9 700 adresses/s sur un seul cœur, et de l'ordre de 7 500 par cœur quand tous travaillent (45 300 sur les 6 cœurs performance d'un M1 Pro), avec des adresses en ordre naturel.
- **La mémoire doit garder l'index en cache :** environ 2 Go en plus du reste. Si l'index ne tient pas en cache, chaque recherche attend le disque et le débit s'effondre.

### Sécuriser

> ⚠️ Comme addok, le serveur fait confiance à ses clients : il n'impose ni authentification ni limite de taille aux fichiers envoyés.

- **Gardez-le sur un réseau interne.**
- **S'il doit être exposé au-delà,** placez devant lui un frontal (nginx, Traefik, une passerelle d'API) qui authentifie les clients, limite la taille des requêtes et leur débit, et termine le TLS.

### Superviser

- **Disponibilité :** `GET /health` répond même sous pleine charge. Vérifiez `status` et que `documents` correspond à l'index attendu.
- **Avertissements :** le serveur écrit dans son journal chaque requête qui laisse des lignes sans résultat (voir [`X-Addok-Warning`](#len-tête-x-addok-warning)).

### Tirer le meilleur des performances

- **En local, préférez `addok-cli batch`.** Il n'y a ni réseau ni découpage à gérer, et il utilise tous les cœurs : environ 45 000 lignes/s sur un M1 Pro à 8 cœurs, lecture et écriture comprises.
- **En HTTP, envoyez plusieurs requêtes à la fois.** Chaque requête n'occupe qu'un cœur. Envoyez des morceaux d'environ 1 000 lignes, avec au moins autant de requêtes en vol que de cœurs côté serveur ; quatre fois plus ne nuit pas. Ainsi réglé, `/search/csv` dépasse 40 000 lignes/s.
- **Gardez les lignes dans leur ordre naturel.** Des adresses voisines (même département, même commune) partagent les mêmes données d'index, qui restent alors en cache. Géocoder des lignes triées par département est nettement plus rapide que géocoder les mêmes lignes mélangées : envoyez donc des morceaux de lignes **consécutives**, pas des lignes tirées au hasard.
- **Partagez la machine avec `--cores`** quand d'autres traitements tournent à côté.

---

## 11. Passer d'addok à addok-rs

addok-rs est fait pour remplacer un addok qui géocode par lots avec `/search/csv`, sans toucher au client.

1. **Construisez l'index** à partir de l'export NDJSON de la BAN ([section 3](#3-construire-lindex)). L'archive `addok.db` et `dump.rdb` d'addok ne sert pas.
2. **Lancez le serveur** sur le port 7878, comme addok ([section 5](#5-faire-tourner-le-serveur--addok-cli-serve)).
3. **Pointez le client vers addok-rs.** `/search/csv` accepte les mêmes paramètres et rend les mêmes colonnes, dans le même ordre et au même format. Seule `result_street`, toujours vide chez addok-csv, disparaît.
4. **Recalibrez votre seuil de confiance.** addok-rs suit addok 1.3.2. Si vous veniez de l'image `etalab/addok` (addok 1.0.3, de 2022), les scores bougent  : trois ans de corrections amont séparent les deux (66 types de voie reconnus au lieu d'environ 38, libellés des communes fusionnées, règles phonétiques réécrites). Mesurez le seuil sur un échantillon vérifié de vos propres données.
5. **Retirez les paramètres non portés.** `lat` et `lon` sont refusés avec une erreur 400, plutôt qu'ignorés en silence. Les filtres (`type`, `citycode`, `postcode`), eux, fonctionnent, alors qu'addok-csv 1.1.0 échouait dessus.
6. **Retirez les contournements devenus inutiles :** le champ `delimiter` envoyé pour éviter un séparateur mal deviné, et le nettoyage des lignes trop longues fait pour ne pas perdre tout un fichier.
7. **Une fois la bascule faite,** Redis et SQLite peuvent être arrêtés.

Ensuite, deux pas facultatifs :

- **Passer à `/batch`** pour échanger du Parquet typé plutôt que du CSV ([section 6](#post-batch)).
- **Activer les améliorations** : le [repli sur la commune](#code-postal-faux--le-repli-sur-la-commune) et le [numéro découpé](#le-numéro-découpé--numéro-complément-forme-courte). addok ignore les champs `postcode_fallback` et `result_columns`, ce qui permet de les envoyer aux deux moteurs pendant la transition.

---

## 12. Dépannage

| Message | Cause | Que faire |
|---|---|---|
| `--cores 9: this machine has 8 cores available` | Plus de cœurs demandés que la machine (ou le conteneur) n'en offre | Choisir un nombre entre 1 et celui indiqué |
| `--cores takes a whole number from 1 to 8, not "0"` | Valeur non entière, nulle ou négative | Donner un entier positif |
| `index without a WordTable section: written by an older addok-rs, or damaged; rebuild it with addok-cli build` | Index construit par une version plus ancienne d'addok-rs, ou fichier abîmé | Relancer `addok-cli build` |
| `not an addok-rs index` | Le fichier n'est pas un index addok-rs | Vérifier le chemin passé à `serve` ou `batch` |
| `adresses-addok-france.ndjson.gz, line 3: …` | Ligne illisible dans l'export de la BAN, souvent un téléchargement interrompu | Télécharger l'export à nouveau ; l'ancien index est intact |
| `…: name its format with --input-format or --output-format` | Extension de fichier non reconnue | Préciser `--input-format` ou `--output-format` |
| `no column "adresse"` | Une colonne de `--columns` (ou du champ `columns`) n'existe pas dans le fichier | Vérifier les noms et la casse ; pour un CSV, vérifier aussi le séparateur |
| `the CSV is not UTF-8` | CSV d'entrée dans un autre encodage (Windows-1252, par exemple) | Le convertir en UTF-8 |
| 400 `Cannot found column 'adresse' in columns ['adresse,ville,code_postal']` | `/search/csv` : une colonne demandée n'existe pas dans le fichier, quel que soit le séparateur usuel essayé | Vérifier les noms de colonnes ; pour un séparateur inhabituel, l'indiquer avec le champ `delimiter` |
| 400 `Cannot found column 'cp' in columns [...]`, ou `no column "cp"` | Un filtre nomme une colonne que le fichier n'a pas | Vérifier le nom de la colonne donnée au filtre |
| 400 `Unsupported parameter "lat"` | Position non portée | Retirer le paramètre ([Limites actuelles](#13-limites-actuelles)) |
| En-tête `X-Addok-Warning: query_too_long; …; rows=12`, ou `warning: 1 row longer than 200 characters left without a result: row 12` | Une ligne dépasse 200 caractères : elle reste sans résultat, les autres sont géocodées | Nettoyer la ligne indiquée, souvent du texte parasite (lorem ipsum, données de test) |
| `/data/ban.addok: No such file or directory`, le conteneur s'arrête aussitôt | Pas d'index à la racine du dossier monté sur `/data`, ou sous un autre nom | Vérifier le chemin du volume et le nom du fichier ([Avec Docker](#avec-docker)) |
| `Permission denied` en construisant l'index avec Docker | Le dossier monté n'est pas accessible en écriture à l'utilisateur du conteneur | Ajouter `--user "$(id -u):$(id -g)"` |
| Débit bien plus faible qu'attendu | Requêtes envoyées une par une, lignes mélangées, ou index qui ne tient pas en cache | Voir [Tirer le meilleur des performances](#tirer-le-meilleur-des-performances) |

---

## 13. Limites actuelles

Ces fonctions d'addok ne sont pas encore portées :

- **la recherche autour d'un point** (`lat`, `lon`) et **le géocodage inverse** (`/reverse`, `/reverse/csv`) ;
- **le point d'entrée JSON** `/search`, et l'autocomplétion.

addok-rs ne géocode que les adresses françaises de la BAN : son pipeline est celui qu'addok utilise pour la France, écrit en dur.
