<?php
// Persistent visit counter. GET reads it, POST adds a visit.
header('Content-Type: application/json');
$dsn = sprintf('mysql:host=%s;port=%s;dbname=%s;charset=utf8mb4', getenv('DB_HOST'), getenv('DB_PORT'), getenv('DB_DATABASE'));
try {
    $pdo = new PDO($dsn, getenv('DB_USERNAME'), getenv('DB_PASSWORD'), [PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION]);
    $pdo->exec('CREATE TABLE IF NOT EXISTS visits (id INT AUTO_INCREMENT PRIMARY KEY, at TIMESTAMP DEFAULT CURRENT_TIMESTAMP)');
    if ($_SERVER['REQUEST_METHOD'] === 'POST') {
        $pdo->exec('INSERT INTO visits () VALUES ()');
    }
    $count = (int) $pdo->query('SELECT COUNT(*) FROM visits')->fetchColumn();
    echo json_encode(['database' => getenv('DB_DATABASE'), 'visits' => $count]);
} catch (PDOException $e) {
    http_response_code(500);
    echo json_encode(['error' => $e->getMessage()]);
}
