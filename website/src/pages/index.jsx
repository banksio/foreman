import React from 'react';
import Link from '@docusaurus/Link';
import Layout from '@theme/Layout';
import styles from './index.module.css';

const description = 'Updates, reboots, and scheduled maintenance for Debian-family hosts.';

export default function Home() {
  return (
    <Layout description={description}>
      <main className={styles.hero}>
        <div className={styles.content}>
          <h1>Foreman</h1>
          <p>{description}</p>
          <div className={styles.actions}>
            <Link className={styles.start} to="/quick-start/">
              Get started
            </Link>
            <Link className={styles.github} href="https://github.com/banksio/foreman">
              GitHub
            </Link>
          </div>
        </div>
      </main>
    </Layout>
  );
}
