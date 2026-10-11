FROM dunglas/frankenphp:latest
RUN install-php-extensions mysqli pdo_mysql gd intl exif zip bcmath
